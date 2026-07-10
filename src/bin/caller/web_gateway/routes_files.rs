//! The files surface of the gateway: dashboard fs API (stat/list/read/
//! mkdir/write/rename/delete) with its request types and IAM-checked
//! apply fns, the current-session upload store endpoints, and the
//! dashboard source-viewer (local file rendering) with its embedded
//! CSS/JS.

use super::*;

pub(crate) const FS_LIST_LIMIT: usize = 500;

pub(crate) const SOURCE_VIEWER_MAX_BYTES: u64 = 5 * 1024 * 1024;

pub(crate) const DASHBOARD_IMAGE_MAX_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub(crate) struct FsPathStatus {
    input: String,
    path: String,
    exists: bool,
    is_dir: bool,
    is_file: bool,
    readable: bool,
    size: Option<u64>,
    modified_ms: Option<u64>,
    parent: Option<String>,
    parent_exists: bool,
    parent_is_dir: bool,
    nearest_existing_parent: Option<String>,
    can_create: bool,
}

#[derive(Debug, Serialize)]
pub(crate) struct FsListEntry {
    name: String,
    path: String,
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
    hidden: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct FsMkdirRequest {
    path: String,
}

/// Body of `POST /api/fs/write` — the dashboard editor's save request.
///
/// Exactly one of `content` (UTF-8 text) or `content_b64` (raw bytes) carries
/// the new file contents. Every write must state its precondition: an
/// `expected_sha256` of the bytes the client last read (optimistic
/// concurrency), `create_new` for files that must not exist yet, or an
/// explicit `force` to overwrite unconditionally. A write with no
/// precondition is rejected so nothing clobbers a changed file silently.
#[derive(Debug, Deserialize)]
pub(crate) struct FsWriteRequest {
    path: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    content_b64: Option<String>,
    #[serde(default)]
    expected_sha256: Option<String>,
    #[serde(default)]
    create_new: bool,
    #[serde(default)]
    force: bool,
}

/// Body of `POST /api/fs/rename` — move/rename a file or directory. Both
/// `from` and `to` must pass write-scope authorization (removing an entry is
/// a write at the source; creating one is a write at the destination). A
/// rename never replaces an existing destination — fail closed and let the
/// client delete explicitly first.
#[derive(Debug, Deserialize)]
pub(crate) struct FsRenameRequest {
    from: String,
    to: String,
}

/// Body of `POST /api/fs/delete`. Files and symlinks (the link itself, never
/// its target) delete unconditionally; directories must be empty unless the
/// client states `recursive: true`.
#[derive(Debug, Deserialize)]
pub(crate) struct FsDeleteRequest {
    path: String,
    #[serde(default)]
    recursive: bool,
}

/// Hard cap on individual uploaded file size. Prevents a rogue or mistaken
/// upload (e.g. someone dragging a multi-GB video file) from OOMing the
/// daemon or filling the session dir. Plumbed through the streaming reader
/// so we bail before reading the full body.
///
/// Picked to cover common real uploads (PDFs, CSVs, source archives,
/// annotated screenshots) without accepting arbitrary blobs. Can be made
/// configurable later via `[upload] max_size_mb` in intendant.toml.
pub(crate) const UPLOAD_MAX_BYTES: usize = 100 * 1024 * 1024;

/// Session-dir stand-in used when no session log is active:
/// `<project>/.intendant/pending_uploads`, or the equivalent directory in
/// the daemon-global store on projectless daemons.
pub(crate) fn pending_upload_session_dir(
    scope: &crate::global_store::StoreScope,
) -> std::path::PathBuf {
    scope.store_base().join("pending_uploads")
}

pub(crate) fn current_upload_commit_response_body(
    project_root: Option<&std::path::Path>,
    session_log: Option<&Arc<Mutex<crate::session_log::SessionLog>>>,
    daemon_session_id: Option<&str>,
    name: &str,
    mime: &str,
    requested_destination: crate::upload_store::UploadDestination,
    tmp: tempfile::NamedTempFile,
    size: usize,
    bus: &crate::event::EventBus,
) -> (&'static str, String) {
    let scope = crate::global_store::StoreScope::resolve(project_root);

    let (session_dir, session_id) = if let Some(slog) = session_log {
        match slog.lock() {
            Ok(l) => (l.dir().to_path_buf(), l.session_id().to_string()),
            Err(_) => {
                return (
                    "500 Internal Server Error",
                    serde_json::json!({ "error": "session log lock poisoned" }).to_string(),
                );
            }
        }
    } else {
        (
            pending_upload_session_dir(&scope),
            daemon_session_id.unwrap_or("pending").to_string(),
        )
    };
    let destination = effective_upload_destination(requested_destination, session_log.is_some());
    match crate::upload_store::commit_upload(
        tmp,
        name,
        mime,
        size as u64,
        destination,
        &session_dir,
        &session_id,
        &scope,
    ) {
        Ok(descriptor) => {
            bus.send(crate::event::AppEvent::UploadReady {
                descriptor: descriptor.clone(),
            });
            (
                "200 OK",
                serde_json::to_string(&descriptor).unwrap_or_else(|_| "{}".to_string()),
            )
        }
        Err(e) => (
            "500 Internal Server Error",
            serde_json::json!({ "error": format!("commit upload: {e}") }).to_string(),
        ),
    }
}

pub(crate) fn current_upload_delete_response_body(
    project_root: Option<&std::path::Path>,
    session_dir: Option<&std::path::Path>,
    id: &str,
) -> (&'static str, String, Option<String>) {
    let scope = crate::global_store::StoreScope::resolve(project_root);
    let id = id.trim();
    if id.is_empty() {
        return (
            "400 Bad Request",
            serde_json::json!({ "error": "missing upload id" }).to_string(),
            None,
        );
    }
    let pending_dir;
    let session_dir = match session_dir {
        Some(dir) => dir,
        None => {
            pending_dir = pending_upload_session_dir(&scope);
            pending_dir.as_path()
        }
    };
    match crate::upload_store::delete_upload(id, session_dir, &scope) {
        Ok(_) => (
            "200 OK",
            serde_json::json!({ "ok": true }).to_string(),
            Some(id.to_string()),
        ),
        Err(e) => (
            "500 Internal Server Error",
            serde_json::json!({ "error": format!("delete: {e}") }).to_string(),
            None,
        ),
    }
}

pub(crate) fn dashboard_source_request_from_line(
    request_line: &str,
) -> Option<DashboardSourceRequest> {
    if !request_line.starts_with("GET ") {
        return None;
    }
    let path_token = request_line.split_whitespace().nth(1)?;
    let path_part = path_token.split('?').next().unwrap_or(path_token);
    if path_part.is_empty() || path_part == "/" {
        return None;
    }
    let decoded = url_path_decode(path_part);
    if decoded.contains('\0') {
        return None;
    }

    let exact_path = dashboard_url_path_to_fs_path(&decoded);
    if source_viewer_file_candidate(&exact_path) {
        return Some(DashboardSourceRequest {
            path: exact_path,
            line: None,
        });
    }

    let (without_line, line) = split_source_line_suffix(&decoded)?;
    let source_path = dashboard_url_path_to_fs_path(without_line);
    if source_viewer_file_candidate(&source_path) {
        return Some(DashboardSourceRequest {
            path: source_path,
            line: Some(line),
        });
    }
    None
}

pub(crate) fn source_viewer_file_candidate(path: &Path) -> bool {
    path.is_absolute()
        && std::fs::metadata(path)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
}

pub(crate) fn dashboard_local_file_response(
    request_line: &str,
) -> Option<DashboardLocalFileResponse> {
    let request = dashboard_source_request_from_line(request_line)?;
    if let Some(content_type) = dashboard_image_content_type(&request.path) {
        Some(render_dashboard_image_file_response(request, content_type))
    } else {
        let (status, body) = render_dashboard_source_viewer_response(request);
        Some(DashboardLocalFileResponse::Html { status, body })
    }
}

pub(crate) fn effective_upload_destination(
    requested: crate::upload_store::UploadDestination,
    _has_active_session: bool,
) -> crate::upload_store::UploadDestination {
    requested
}

/// Decode percent escapes in an HTTP path segment. Unlike query-string
/// decoding, `+` is a literal plus in paths.
pub(crate) fn url_path_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let h = &bytes[i + 1..i + 3];
                match std::str::from_utf8(h)
                    .ok()
                    .and_then(|hs| u8::from_str_radix(hs, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DashboardSourceRequest {
    path: PathBuf,
    line: Option<usize>,
}

pub(crate) enum DashboardLocalFileResponse {
    Html {
        status: &'static str,
        body: String,
    },
    Bytes {
        status: &'static str,
        content_type: &'static str,
        bytes: Vec<u8>,
    },
}

pub(crate) fn dashboard_url_path_to_fs_path(decoded: &str) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(rest) = decoded.strip_prefix('/') {
            if looks_like_windows_drive_path(rest) {
                return PathBuf::from(rest);
            }
        }
    }
    PathBuf::from(decoded)
}

#[cfg(windows)]
pub(crate) fn looks_like_windows_drive_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
        && bytes[0].is_ascii_alphabetic()
}

pub(crate) fn split_source_line_suffix(raw: &str) -> Option<(&str, usize)> {
    let (path, line_raw) = raw.rsplit_once(':')?;
    if path.is_empty() || line_raw.is_empty() || !line_raw.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let line = line_raw.parse::<usize>().ok()?;
    if line == 0 {
        return None;
    }
    Some((path, line))
}

pub(crate) fn render_dashboard_image_file_response(
    request: DashboardSourceRequest,
    content_type: &'static str,
) -> DashboardLocalFileResponse {
    let display_path = std::fs::canonicalize(&request.path).unwrap_or(request.path.clone());
    let display_path_str = display_path.to_string_lossy().to_string();
    let metadata = match std::fs::metadata(&display_path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => {
            return DashboardLocalFileResponse::Html {
                status: "404 Not Found",
                body: render_dashboard_source_error_html(
                    &display_path_str,
                    "Not a file",
                    "The requested path is not a regular file.",
                ),
            }
        }
        Err(err) => {
            return DashboardLocalFileResponse::Html {
                status: "404 Not Found",
                body: render_dashboard_source_error_html(
                    &display_path_str,
                    "File not found",
                    &format!("Could not read file metadata: {err}"),
                ),
            }
        }
    };

    if metadata.len() > DASHBOARD_IMAGE_MAX_BYTES {
        return DashboardLocalFileResponse::Html {
            status: "413 Payload Too Large",
            body: render_dashboard_source_error_html(
                &display_path_str,
                "Image too large",
                &format!(
                    "Image preview is limited to {} bytes; this file is {} bytes.",
                    DASHBOARD_IMAGE_MAX_BYTES,
                    metadata.len()
                ),
            ),
        };
    }

    match std::fs::read(&display_path) {
        Ok(bytes) => DashboardLocalFileResponse::Bytes {
            status: "200 OK",
            content_type,
            bytes,
        },
        Err(err) => DashboardLocalFileResponse::Html {
            status: "500 Internal Server Error",
            body: render_dashboard_source_error_html(
                &display_path_str,
                "Read failed",
                &format!("Could not read the file: {err}"),
            ),
        },
    }
}

#[cfg(test)]
pub(crate) fn dashboard_source_viewer_response(
    request_line: &str,
) -> Option<(&'static str, String)> {
    let request = dashboard_source_request_from_line(request_line)?;
    Some(render_dashboard_source_viewer_response(request))
}

pub(crate) fn dashboard_image_content_type(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => Some("image/png"),
        "jpg" | "jpeg" | "jfif" | "pjpeg" | "pjp" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "avif" => Some("image/avif"),
        "bmp" => Some("image/bmp"),
        "ico" => Some("image/x-icon"),
        _ => None,
    }
}

pub(crate) fn render_dashboard_source_viewer_response(
    request: DashboardSourceRequest,
) -> (&'static str, String) {
    let display_path = std::fs::canonicalize(&request.path).unwrap_or(request.path.clone());
    let display_path_str = display_path.to_string_lossy().to_string();
    let metadata = match std::fs::metadata(&display_path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => {
            return (
                "404 Not Found",
                render_dashboard_source_error_html(
                    &display_path_str,
                    "Not a file",
                    "The requested path is not a regular file.",
                ),
            )
        }
        Err(err) => {
            return (
                "404 Not Found",
                render_dashboard_source_error_html(
                    &display_path_str,
                    "File not found",
                    &format!("Could not read file metadata: {err}"),
                ),
            )
        }
    };

    if metadata.len() > SOURCE_VIEWER_MAX_BYTES {
        return (
            "413 Payload Too Large",
            render_dashboard_source_error_html(
                &display_path_str,
                "File too large",
                &format!(
                    "Source viewer is limited to {} bytes; this file is {} bytes.",
                    SOURCE_VIEWER_MAX_BYTES,
                    metadata.len()
                ),
            ),
        );
    }

    let bytes = match std::fs::read(&display_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            return (
                "500 Internal Server Error",
                render_dashboard_source_error_html(
                    &display_path_str,
                    "Read failed",
                    &format!("Could not read the file: {err}"),
                ),
            )
        }
    };

    if bytes.contains(&0) {
        return (
            "415 Unsupported Media Type",
            render_dashboard_source_error_html(
                &display_path_str,
                "Binary file",
                "The requested file appears to be binary and cannot be rendered as source.",
            ),
        );
    }

    let text = String::from_utf8_lossy(&bytes);
    let language = source_viewer_language(&display_path);
    use base64::Engine as _;
    let content_b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    (
        "200 OK",
        render_dashboard_source_viewer_html(
            &display_path_str,
            request.line,
            language,
            &content_b64,
            bytes.len(),
        ),
    )
}

pub(crate) fn source_viewer_language(path: &Path) -> &'static str {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match file_name.as_str() {
        "cargo.toml" => return "toml",
        "makefile" | "dockerfile" => return "shell",
        _ => {}
    }

    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "rs" => "rust",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "ts" | "tsx" => "typescript",
        "py" => "python",
        "rb" => "ruby",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "php" => "php",
        "sh" | "bash" | "zsh" | "fish" | "ps1" => "shell",
        "json" | "jsonl" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "markdown" => "markdown",
        "css" | "scss" | "sass" => "css",
        "html" | "htm" => "html",
        "xml" => "xml",
        "sql" => "sql",
        _ => "",
    }
}

pub(crate) fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) fn render_dashboard_source_error_html(path: &str, title: &str, message: &str) -> String {
    let mut html = String::new();
    html.push_str("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>");
    html.push_str(&html_escape(title));
    html.push_str("</title><style>");
    html.push_str(SOURCE_VIEWER_BASE_CSS);
    html.push_str(
        "</style></head><body><header><div class=\"source-kicker\">Intendant source</div><h1>",
    );
    html.push_str(&html_escape(title));
    html.push_str("</h1><div class=\"source-path\">");
    html.push_str(&html_escape(path));
    html.push_str("</div></header><main class=\"source-error\"><p>");
    html.push_str(&html_escape(message));
    html.push_str("</p><a href=\"/\">Dashboard</a></main></body></html>");
    html
}

pub(crate) fn render_dashboard_source_viewer_html(
    path: &str,
    line: Option<usize>,
    language: &str,
    content_b64: &str,
    byte_len: usize,
) -> String {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path);
    let title = match line {
        Some(line) => format!("{file_name}:{line}"),
        None => file_name.to_string(),
    };
    let data_json = serde_json::json!({
        "path": path,
        "line": line,
        "language": language,
        "bytes": byte_len,
        "content_b64": content_b64,
    })
    .to_string()
    .replace("</", "<\\/");

    let mut html = String::new();
    html.push_str("<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>");
    html.push_str(&html_escape(&title));
    html.push_str("</title><style>");
    html.push_str(SOURCE_VIEWER_BASE_CSS);
    html.push_str(SOURCE_VIEWER_CODE_CSS);
    html.push_str("</style></head><body><header><div class=\"source-topline\"><div><div class=\"source-kicker\">Intendant source</div><h1 title=\"");
    html.push_str(&html_escape(path));
    html.push_str("\">");
    html.push_str(&html_escape(&title));
    html.push_str("</h1></div><a class=\"source-dashboard\" href=\"/\">Dashboard</a></div><div class=\"source-path\">");
    html.push_str(&html_escape(path));
    html.push_str("</div><div class=\"source-meta\"><span>");
    if language.is_empty() {
        html.push_str("text");
    } else {
        html.push_str(&html_escape(language));
    }
    html.push_str("</span><span id=\"source-line-count\">loading</span><span>");
    html.push_str(&byte_len.to_string());
    html.push_str(" bytes</span></div></header><main><pre id=\"source-code\" class=\"source-code\" aria-label=\"source file\"></pre></main><script>const SOURCE_DATA = ");
    html.push_str(&data_json);
    html.push_str(";</script><script>");
    html.push_str(SOURCE_VIEWER_JS);
    html.push_str("</script></body></html>");
    html
}

pub(crate) const SOURCE_VIEWER_BASE_CSS: &str = r#"
:root {
  color-scheme: dark;
  --base: #1e1e2e;
  --mantle: #181825;
  --crust: #11111b;
  --surface0: #313244;
  --surface1: #45475a;
  --overlay0: #6c7086;
  --subtext0: #a6adc8;
  --text: #cdd6f4;
  --blue: #89b4fa;
  --sapphire: #74c7ec;
  --green: #a6e3a1;
  --yellow: #f9e2af;
  --peach: #fab387;
  --red: #f38ba8;
  --mauve: #cba6f7;
  --font-mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", monospace;
  --font-sans: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
}
* { box-sizing: border-box; }
body {
  margin: 0;
  min-height: 100vh;
  color: var(--text);
  background: var(--base);
  font-family: var(--font-sans);
}
header {
  position: sticky;
  top: 0;
  z-index: 2;
  padding: 14px 18px 12px;
  background: color-mix(in srgb, var(--mantle) 94%, transparent);
  border-bottom: 1px solid var(--surface0);
  backdrop-filter: blur(10px);
}
.source-topline {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 16px;
}
.source-kicker {
  color: var(--subtext0);
  font-size: 11px;
  font-weight: 700;
  letter-spacing: 0;
  text-transform: uppercase;
}
h1 {
  margin: 2px 0 0;
  font-size: 18px;
  line-height: 1.25;
  font-weight: 700;
  word-break: break-word;
}
.source-dashboard {
  flex: 0 0 auto;
  color: var(--crust);
  background: var(--blue);
  border: 0;
  border-radius: 5px;
  padding: 6px 10px;
  font-size: 12px;
  font-weight: 700;
  text-decoration: none;
}
.source-path {
  margin-top: 8px;
  color: var(--subtext0);
  font-family: var(--font-mono);
  font-size: 12px;
  line-height: 1.45;
  overflow-wrap: anywhere;
}
.source-meta {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
  margin-top: 9px;
}
.source-meta span {
  color: var(--subtext0);
  border: 1px solid var(--surface0);
  border-radius: 4px;
  padding: 2px 7px;
  font-family: var(--font-mono);
  font-size: 11px;
}
.source-error {
  max-width: 860px;
  margin: 48px auto;
  padding: 0 18px;
  color: var(--text);
}
.source-error p {
  margin: 0 0 14px;
  color: var(--subtext0);
  line-height: 1.5;
}
.source-error a {
  color: var(--sapphire);
}
"#;

pub(crate) const SOURCE_VIEWER_CODE_CSS: &str = r#"
main {
  overflow-x: auto;
}
.source-code {
  margin: 0;
  min-width: max-content;
  padding: 10px 0 28px;
  font-family: var(--font-mono);
  font-size: 12px;
  line-height: 1.55;
  tab-size: 2;
}
.source-line {
  display: grid;
  grid-template-columns: 72px minmax(max-content, 1fr);
  min-height: 1.55em;
  scroll-margin-top: 110px;
}
.source-line:hover {
  background: rgba(69, 71, 90, 0.28);
}
.source-line.is-target {
  background: rgba(250, 179, 135, 0.16);
  box-shadow: inset 3px 0 0 var(--peach);
}
.line-no {
  user-select: none;
  padding: 0 12px 0 18px;
  color: var(--overlay0);
  text-align: right;
  text-decoration: none;
}
.source-line:hover .line-no,
.source-line.is-target .line-no {
  color: var(--peach);
}
.line-code {
  display: block;
  padding-right: 24px;
  color: var(--text);
  white-space: pre;
}
.syntax-comment { color: var(--overlay0); }
.syntax-string { color: var(--green); }
.syntax-number { color: var(--peach); }
.syntax-keyword { color: var(--mauve); font-weight: 600; }
.syntax-type { color: var(--yellow); }
.syntax-fn { color: var(--blue); }
.syntax-punct { color: var(--subtext0); }
.syntax-op { color: var(--red); }
.syntax-var { color: var(--sapphire); }
.syntax-tag { color: var(--blue); }
.syntax-attr { color: var(--yellow); }
.syntax-md-heading { color: var(--mauve); font-weight: 700; }
.syntax-md-code { color: var(--peach); }
"#;

pub(crate) const SOURCE_VIEWER_JS: &str = r##"
(function () {
  const data = SOURCE_DATA || {};
  const bytes = Uint8Array.from(atob(data.content_b64 || ''), ch => ch.charCodeAt(0));
  const text = new TextDecoder('utf-8').decode(bytes).replace(/\r\n?/g, '\n');
  const lines = text.length ? (text.endsWith('\n') ? text.slice(0, -1).split('\n') : text.split('\n')) : [''];
  const codeEl = document.getElementById('source-code');
  const countEl = document.getElementById('source-line-count');
  const language = String(data.language || '');
  const keywords = new Set(('as async await break case catch class const continue default defer delete do dyn else enum export extends extern false finally fn for from func function if impl import in interface let loop match mod move mut new nil null package priv pub ref return self Self static struct super switch this throw trait true try type typeof undefined unsafe use var where while yield').split(/\s+/));

  function escapeHtml(value) {
    return String(value).replace(/[&<>"']/g, ch => ({
      '&': '&amp;',
      '<': '&lt;',
      '>': '&gt;',
      '"': '&quot;',
      "'": '&#39;'
    })[ch]);
  }

  function span(cls, value) {
    return '<span class="' + cls + '">' + escapeHtml(value) + '</span>';
  }

  function highlightMarkdown(src) {
    let html = escapeHtml(src);
    html = html.replace(/^(\s{0,3}#{1,6})(\s.*)?$/, '<span class="syntax-md-heading">$1</span>$2');
    html = html.replace(/(`[^`]+`)/g, '<span class="syntax-md-code">$1</span>');
    html = html.replace(/(\*\*[^*]+\*\*)/g, '<span class="syntax-keyword">$1</span>');
    return html;
  }

  function highlightJsonLike(src) {
    let html = escapeHtml(src);
    html = html.replace(/(&quot;(?:\\.|[^&])*?&quot;)(\s*:)/g, '<span class="syntax-attr">$1</span>$2');
    html = html.replace(/(:\s*)(&quot;(?:\\.|[^&])*?&quot;)/g, '$1<span class="syntax-string">$2</span>');
    html = html.replace(/\b(true|false|null)\b/g, '<span class="syntax-keyword">$1</span>');
    html = html.replace(/\b(-?\d+(?:\.\d+)?)\b/g, '<span class="syntax-number">$1</span>');
    return html;
  }

  function highlightMarkup(src) {
    let html = escapeHtml(src);
    html = html.replace(/(&lt;\/?)([A-Za-z0-9:_-]+)/g, '$1<span class="syntax-tag">$2</span>');
    html = html.replace(/\b([A-Za-z_:][-A-Za-z0-9_:.]*)(=)/g, '<span class="syntax-attr">$1</span>$2');
    html = html.replace(/(&quot;.*?&quot;|&#39;.*?&#39;)/g, '<span class="syntax-string">$1</span>');
    return html;
  }

  function highlightCss(src) {
    let html = escapeHtml(src);
    html = html.replace(/(\/\*.*?\*\/)/g, '<span class="syntax-comment">$1</span>');
    html = html.replace(/([.#]?[A-Za-z_-][A-Za-z0-9_-]*)(\s*[:{])/g, '<span class="syntax-tag">$1</span>$2');
    html = html.replace(/\b(-?\d+(?:\.\d+)?(?:px|rem|em|%|vh|vw)?)\b/g, '<span class="syntax-number">$1</span>');
    return html;
  }

  function highlightCode(src) {
    let html = '';
    let i = 0;
    const hashComment = ['python', 'ruby', 'shell', 'toml', 'yaml'].includes(language);
    const sqlComment = language === 'sql';
    while (i < src.length) {
      const ch = src[i];
      const next = src[i + 1] || '';
      if (hashComment && ch === '#') {
        html += span('syntax-comment', src.slice(i));
        break;
      }
      if (sqlComment && ch === '-' && next === '-') {
        html += span('syntax-comment', src.slice(i));
        break;
      }
      if (ch === '/' && next === '/') {
        html += span('syntax-comment', src.slice(i));
        break;
      }
      if (ch === '/' && next === '*') {
        const end = src.indexOf('*/', i + 2);
        const stop = end === -1 ? src.length : end + 2;
        html += span('syntax-comment', src.slice(i, stop));
        i = stop;
        continue;
      }
      if (ch === '"' || ch === "'" || ch === '`') {
        const quote = ch;
        let j = i + 1;
        while (j < src.length) {
          if (src[j] === '\\') {
            j += 2;
            continue;
          }
          if (src[j] === quote) {
            j++;
            break;
          }
          j++;
        }
        html += span('syntax-string', src.slice(i, j));
        i = j;
        continue;
      }
      if (/[0-9]/.test(ch) && (i === 0 || !/[A-Za-z0-9_]/.test(src[i - 1] || ''))) {
        let j = i + 1;
        while (j < src.length && /[A-Za-z0-9_.]/.test(src[j])) j++;
        html += span('syntax-number', src.slice(i, j));
        i = j;
        continue;
      }
      if (/[A-Za-z_$]/.test(ch)) {
        let j = i + 1;
        while (j < src.length && /[A-Za-z0-9_$]/.test(src[j])) j++;
        const token = src.slice(i, j);
        const rest = src.slice(j).trimStart();
        if (keywords.has(token)) {
          html += span('syntax-keyword', token);
        } else if (/^[A-Z][A-Za-z0-9_$]*$/.test(token)) {
          html += span('syntax-type', token);
        } else if (rest.startsWith('(')) {
          html += span('syntax-fn', token);
        } else {
          html += escapeHtml(token);
        }
        i = j;
        continue;
      }
      if ('{}[]().,;:'.includes(ch)) {
        html += span('syntax-punct', ch);
      } else if ('+-=*/!&|<>?%'.includes(ch)) {
        html += span('syntax-op', ch);
      } else {
        html += escapeHtml(ch);
      }
      i++;
    }
    return html;
  }

  function highlightLine(src) {
    if (language === 'markdown') return highlightMarkdown(src);
    if (language === 'json') return highlightJsonLike(src);
    if (language === 'html' || language === 'xml') return highlightMarkup(src);
    if (language === 'css') return highlightCss(src);
    return highlightCode(src);
  }

  function targetFromHash() {
    const match = location.hash.match(/^#L?(\d+)$/i);
    return match ? Number(match[1]) : 0;
  }

  function setTarget(line, scroll) {
    document.querySelector('.source-line.is-target')?.classList.remove('is-target');
    if (!line || !Number.isFinite(line)) return;
    const el = document.getElementById('L' + line);
    if (!el) return;
    el.classList.add('is-target');
    if (scroll) requestAnimationFrame(() => el.scrollIntoView({ block: 'center' }));
  }

  codeEl.innerHTML = lines.map((line, index) => {
    const number = index + 1;
    return '<div class="source-line" id="L' + number + '" data-line="' + number + '">' +
      '<a class="line-no" href="#L' + number + '">' + number + '</a>' +
      '<code class="line-code">' + (highlightLine(line) || ' ') + '</code>' +
      '</div>';
  }).join('');
  countEl.textContent = lines.length + (lines.length === 1 ? ' line' : ' lines');
  setTarget(Number(data.line || 0) || targetFromHash(), true);
  window.addEventListener('hashchange', () => setTarget(targetFromHash(), true));
})();
"##;

pub(crate) fn expand_dashboard_fs_path(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    let path = if trimmed.is_empty() || trimmed == "~" {
        dirs::home_dir().ok_or_else(|| "could not resolve home directory".to_string())?
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        dirs::home_dir()
            .ok_or_else(|| "could not resolve home directory".to_string())?
            .join(rest)
    } else {
        PathBuf::from(trimmed)
    };
    if !path.is_absolute() {
        return Err(format!(
            "path must be absolute or start with ~/ (got {})",
            trimmed
        ));
    }
    Ok(path)
}

pub(crate) fn nearest_existing_parent(path: &Path) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

pub(crate) fn inspect_dashboard_fs_path(raw: &str) -> Result<FsPathStatus, String> {
    let path = expand_dashboard_fs_path(raw)?;
    let metadata = std::fs::metadata(&path).ok();
    let exists = metadata.is_some();
    let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
    let is_file = metadata.as_ref().map(|m| m.is_file()).unwrap_or(false);
    let readable = if is_dir {
        std::fs::read_dir(&path).is_ok()
    } else if is_file {
        std::fs::File::open(&path).is_ok()
    } else {
        false
    };
    let display_path = if exists {
        std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone())
    } else {
        path.clone()
    };
    let parent = path.parent().map(|p| p.to_string_lossy().to_string());
    let parent_metadata = path.parent().and_then(|p| std::fs::metadata(p).ok());
    let nearest = nearest_existing_parent(&path);
    let nearest_is_dir = nearest
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.is_dir())
        .unwrap_or(false);
    Ok(FsPathStatus {
        input: raw.trim().to_string(),
        path: display_path.to_string_lossy().to_string(),
        exists,
        is_dir,
        is_file,
        readable,
        size: metadata.as_ref().map(|m| m.len()),
        modified_ms: metadata
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(system_time_unix_ms),
        parent,
        parent_exists: parent_metadata.is_some(),
        parent_is_dir: parent_metadata.map(|m| m.is_dir()).unwrap_or(false),
        nearest_existing_parent: nearest.map(|p| p.to_string_lossy().to_string()),
        can_create: !exists && nearest_is_dir,
    })
}

pub(crate) fn list_dashboard_fs_dir(raw: &str) -> Result<serde_json::Value, String> {
    let path = expand_dashboard_fs_path(raw)?;
    let canonical = std::fs::canonicalize(&path)
        .map_err(|e| format!("{} is not accessible: {}", path.display(), e))?;
    if !canonical.is_dir() {
        return Err(format!("{} is not a directory", canonical.display()));
    }
    let read_dir = std::fs::read_dir(&canonical)
        .map_err(|e| format!("could not read {}: {}", canonical.display(), e))?;
    let mut entries = Vec::new();
    for entry in read_dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let file_type = entry.file_type().ok();
        let metadata = entry.metadata().ok();
        let is_dir = metadata.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let is_file = metadata.as_ref().map(|m| m.is_file()).unwrap_or(false);
        entries.push(FsListEntry {
            hidden: name.starts_with('.'),
            name,
            path: entry.path().to_string_lossy().to_string(),
            is_dir,
            is_file,
            is_symlink: file_type.map(|t| t.is_symlink()).unwrap_or(false),
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let truncated = entries.len() > FS_LIST_LIMIT;
    entries.truncate(FS_LIST_LIMIT);
    let parent = canonical.parent().map(|p| p.to_string_lossy().to_string());
    Ok(serde_json::json!({
        "path": canonical.to_string_lossy().to_string(),
        "parent": parent,
        "home": dirs::home_dir().map(|p| p.to_string_lossy().to_string()),
        "entries": entries,
        "truncated": truncated,
    }))
}

#[derive(Debug)]
pub(crate) struct DashboardFsReadResponse {
    filename: String,
    content_type: String,
    total_size: u64,
    range_start: u64,
    range_end: u64,
    partial: bool,
    bytes: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct DashboardFsReadError {
    status: String,
    message: String,
    total_size: Option<u64>,
}

impl DashboardFsReadError {
    pub(crate) fn new(status: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            message: message.into(),
            total_size: None,
        }
    }

    pub(crate) fn range(
        status: impl Into<String>,
        message: impl Into<String>,
        total_size: u64,
    ) -> Self {
        Self {
            status: status.into(),
            message: message.into(),
            total_size: Some(total_size),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DashboardByteRange {
    start: u64,
    end: u64,
}

pub(crate) fn dashboard_fs_content_type(path: &Path) -> String {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("css") => "text/css; charset=utf-8",
        Some("csv") => "text/csv; charset=utf-8",
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("json") => "application/json",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("md") | Some("markdown") | Some("txt") | Some("toml") | Some("yaml") | Some("yml") => {
            "text/plain; charset=utf-8"
        }
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("wasm") => "application/wasm",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
    .to_string()
}

pub(crate) fn dashboard_fs_io_status(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::NotFound => "404 Not Found",
        std::io::ErrorKind::PermissionDenied => "403 Forbidden",
        _ => "500 Internal Server Error",
    }
}

pub(crate) fn parse_dashboard_range_header(
    raw: &str,
    total_size: u64,
) -> Result<DashboardByteRange, String> {
    let value = raw.trim();
    let Some(unit) = value.get(..6) else {
        return Err("Range must use bytes".to_string());
    };
    if !unit.eq_ignore_ascii_case("bytes=") {
        return Err("Range must use bytes".to_string());
    }
    let spec = value[6..].trim();
    if spec.contains(',') {
        return Err("multiple byte ranges are not supported".to_string());
    }
    let (start_raw, end_raw) = spec
        .split_once('-')
        .ok_or_else(|| "invalid byte range".to_string())?;
    if total_size == 0 {
        return Err("range is not satisfiable".to_string());
    }
    if start_raw.trim().is_empty() {
        let suffix_len = end_raw
            .trim()
            .parse::<u64>()
            .map_err(|_| "invalid byte range suffix".to_string())?;
        if suffix_len == 0 {
            return Err("range is not satisfiable".to_string());
        }
        let start = total_size.saturating_sub(suffix_len);
        return Ok(DashboardByteRange {
            start,
            end: total_size - 1,
        });
    }
    let start = start_raw
        .trim()
        .parse::<u64>()
        .map_err(|_| "invalid byte range start".to_string())?;
    let end = if end_raw.trim().is_empty() {
        total_size - 1
    } else {
        end_raw
            .trim()
            .parse::<u64>()
            .map_err(|_| "invalid byte range end".to_string())?
            .min(total_size - 1)
    };
    if start >= total_size || end < start {
        return Err("range is not satisfiable".to_string());
    }
    Ok(DashboardByteRange { start, end })
}

pub(crate) fn dashboard_fs_read_file(
    raw: &str,
    range_header: Option<&str>,
) -> Result<DashboardFsReadResponse, DashboardFsReadError> {
    let path = expand_dashboard_fs_path(raw)
        .map_err(|e| DashboardFsReadError::new("400 Bad Request", e))?;
    let canonical = std::fs::canonicalize(&path).map_err(|e| {
        DashboardFsReadError::new(
            dashboard_fs_io_status(&e).to_string(),
            format!("{} is not accessible: {}", path.display(), e),
        )
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|e| {
        DashboardFsReadError::new(
            dashboard_fs_io_status(&e).to_string(),
            format!("{} is not accessible: {}", canonical.display(), e),
        )
    })?;
    if !metadata.is_file() {
        return Err(DashboardFsReadError::new(
            "400 Bad Request".to_string(),
            format!("{} is not a file", canonical.display()),
        ));
    }
    let total_size = metadata.len();
    let requested_range = range_header
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|_| total_size > 0)
        .map(|value| {
            parse_dashboard_range_header(value, total_size).map_err(|message| {
                DashboardFsReadError::range("416 Range Not Satisfiable", message, total_size)
            })
        })
        .transpose()?;
    let (range_start, range_end, partial) = if let Some(range) = requested_range {
        (range.start, range.end.saturating_add(1), true)
    } else {
        (0, total_size, false)
    };
    let read_len = range_end.saturating_sub(range_start);
    let read_len_usize: usize = read_len.try_into().map_err(|_| {
        DashboardFsReadError::new(
            "500 Internal Server Error",
            format!("{} is too large to read", canonical.display()),
        )
    })?;
    let mut file = std::fs::File::open(&canonical).map_err(|e| {
        DashboardFsReadError::new(
            dashboard_fs_io_status(&e).to_string(),
            format!("could not open {}: {}", canonical.display(), e),
        )
    })?;
    if range_start > 0 {
        use std::io::Seek as _;
        file.seek(std::io::SeekFrom::Start(range_start))
            .map_err(|e| {
                DashboardFsReadError::new(
                    "500 Internal Server Error",
                    format!("could not seek {}: {}", canonical.display(), e),
                )
            })?;
    }
    let mut bytes = vec![0u8; read_len_usize];
    if read_len_usize > 0 {
        use std::io::Read as _;
        file.read_exact(&mut bytes).map_err(|e| {
            DashboardFsReadError::new(
                dashboard_fs_io_status(&e).to_string(),
                format!("could not read {}: {}", canonical.display(), e),
            )
        })?;
    }
    let raw_name = canonical
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "download.bin".to_string());
    let filename = crate::upload_store::sanitize_name(&raw_name);
    Ok(DashboardFsReadResponse {
        filename,
        content_type: dashboard_fs_content_type(&canonical),
        total_size,
        range_start,
        range_end,
        partial,
        bytes,
    })
}

pub(crate) fn mkdir_dashboard_fs_path(raw: &str) -> Result<serde_json::Value, (String, String)> {
    let path = expand_dashboard_fs_path(raw).map_err(|e| ("400 Bad Request".to_string(), e))?;
    if path.exists() {
        if path.is_dir() {
            let display = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            return Ok(serde_json::json!({
                "ok": true,
                "created": false,
                "already_exists": true,
                "path": display.to_string_lossy().to_string(),
                "notice": "Directory already exists"
            }));
        }
        return Err((
            "409 Conflict".to_string(),
            format!("{} already exists and is not a directory", path.display()),
        ));
    }
    std::fs::create_dir_all(&path).map_err(|e| {
        (
            "500 Internal Server Error".to_string(),
            format!("failed to create {}: {}", path.display(), e),
        )
    })?;
    let display = std::fs::canonicalize(&path).unwrap_or(path);
    Ok(serde_json::json!({
        "ok": true,
        "created": true,
        "already_exists": false,
        "path": display.to_string_lossy().to_string()
    }))
}

pub(crate) fn system_time_unix_ms(time: std::time::SystemTime) -> Option<u64> {
    time.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

pub(crate) fn fs_sha256_hex(bytes: &[u8]) -> String {
    crate::file_watcher::hex_encode(&crate::file_watcher::sha256_hash(bytes))
}

/// The write half of the dashboard editor. What the caller must have done
/// already: routed the raw request path through
/// `authorize_http_filesystem_access` (HTTP) or
/// `authorize_dashboard_control_method` (tunnel) with
/// `FilesystemWrite`/`Write` — this function performs no IAM checks of its
/// own.
pub(crate) struct FsWriteArgs {
    pub path: String,
    pub expected_sha256: Option<String>,
    pub create_new: bool,
    pub force: bool,
}

/// Extract the write payload from an `FsWriteRequest`, enforcing that exactly
/// one of `content` / `content_b64` is present.
pub(crate) fn fs_write_request_bytes(req: &FsWriteRequest) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    match (&req.content, &req.content_b64) {
        (Some(_), Some(_)) => Err("provide either content or content_b64, not both".to_string()),
        (Some(text), None) => Ok(text.clone().into_bytes()),
        (None, Some(b64)) => base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| "content_b64 is not valid base64".to_string()),
        (None, None) => Err("missing content (or content_b64)".to_string()),
    }
}

/// Write `bytes` to `args.path` atomically (tempfile in the destination
/// directory, fsync, rename — permissions of an existing file are preserved),
/// honouring the request's precondition:
///
/// - `create_new` — the file must not exist yet (`409` `code:"exists"`).
/// - `expected_sha256` — the file must still hash to the value the client
///   last read; otherwise `409` `code:"conflict"` with the current hash so
///   the editor can offer reload/overwrite. A vanished file is `409`
///   `code:"missing"`.
/// - `force` — write unconditionally.
/// - none of the above — `400` `code:"precondition_required"`.
///
/// The hash check and the rename are not one transaction; a concurrent
/// writer can still slip between them (the same window every editor has).
/// The precondition exists to catch the common case — the file changed while
/// it sat open in a dashboard buffer — not to serialize writers.
pub(crate) fn apply_dashboard_fs_write(
    args: &FsWriteArgs,
    bytes: &[u8],
) -> (String, serde_json::Value) {
    if bytes.len() > UPLOAD_MAX_BYTES {
        return (
            "413 Payload Too Large".to_string(),
            serde_json::json!({
                "error": format!(
                    "content too large: {} bytes (cap is {})",
                    bytes.len(),
                    UPLOAD_MAX_BYTES
                )
            }),
        );
    }
    let path = match expand_dashboard_fs_path(&args.path) {
        Ok(path) => path,
        Err(e) => {
            return (
                "400 Bad Request".to_string(),
                serde_json::json!({ "error": e }),
            )
        }
    };
    let metadata = std::fs::metadata(&path).ok();
    let (target, existed) = if let Some(metadata) = metadata {
        if !metadata.is_file() {
            return (
                "400 Bad Request".to_string(),
                serde_json::json!({
                    "error": format!("{} is not a regular file", path.display())
                }),
            );
        }
        match std::fs::canonicalize(&path) {
            Ok(canonical) => (canonical, true),
            Err(e) => {
                return (
                    dashboard_fs_io_status(&e).to_string(),
                    serde_json::json!({
                        "error": format!("{} is not accessible: {e}", path.display())
                    }),
                )
            }
        }
    } else {
        let Some(name) = path.file_name().map(|n| n.to_os_string()) else {
            return (
                "400 Bad Request".to_string(),
                serde_json::json!({
                    "error": format!("{} has no file name", path.display())
                }),
            );
        };
        let Some(parent) = path.parent() else {
            return (
                "400 Bad Request".to_string(),
                serde_json::json!({
                    "error": format!("{} has no parent directory", path.display())
                }),
            );
        };
        let canonical_parent = match std::fs::canonicalize(parent) {
            Ok(canonical) => canonical,
            Err(_) => {
                return (
                    "404 Not Found".to_string(),
                    serde_json::json!({
                        "error": format!(
                            "parent directory {} does not exist — create it first",
                            parent.display()
                        ),
                        "code": "missing_parent",
                    }),
                )
            }
        };
        if !canonical_parent.is_dir() {
            return (
                "400 Bad Request".to_string(),
                serde_json::json!({
                    "error": format!("{} is not a directory", canonical_parent.display())
                }),
            );
        }
        (canonical_parent.join(name), false)
    };

    if args.create_new {
        if existed {
            return (
                "409 Conflict".to_string(),
                serde_json::json!({
                    "error": format!("{} already exists", target.display()),
                    "code": "exists",
                }),
            );
        }
    } else if let Some(expected) = args
        .expected_sha256
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if !existed {
            return (
                "409 Conflict".to_string(),
                serde_json::json!({
                    "error": format!("{} no longer exists on disk", target.display()),
                    "code": "missing",
                }),
            );
        }
        let current = match std::fs::read(&target) {
            Ok(current) => current,
            Err(e) => {
                return (
                    dashboard_fs_io_status(&e).to_string(),
                    serde_json::json!({
                        "error": format!("could not read {}: {e}", target.display())
                    }),
                )
            }
        };
        let current_sha256 = fs_sha256_hex(&current);
        if !current_sha256.eq_ignore_ascii_case(expected) {
            let modified_ms = std::fs::metadata(&target)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(system_time_unix_ms);
            return (
                "409 Conflict".to_string(),
                serde_json::json!({
                    "error": format!("{} changed on disk since it was read", target.display()),
                    "code": "conflict",
                    "current_sha256": current_sha256,
                    "size": current.len(),
                    "modified_ms": modified_ms,
                }),
            );
        }
    } else if !args.force {
        return (
            "400 Bad Request".to_string(),
            serde_json::json!({
                "error": "write requires expected_sha256, create_new, or force",
                "code": "precondition_required",
            }),
        );
    }

    let Some(dir) = target.parent() else {
        return (
            "400 Bad Request".to_string(),
            serde_json::json!({
                "error": format!("{} has no parent directory", target.display())
            }),
        );
    };
    let write_result = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        let mut tmp = tempfile::Builder::new()
            .prefix(".intendant-fswrite-")
            .suffix(".tmp")
            .tempfile_in(dir)?;
        tmp.write_all(bytes)?;
        tmp.as_file_mut().sync_all()?;
        if existed {
            if let Ok(current) = std::fs::metadata(&target) {
                let _ = std::fs::set_permissions(tmp.path(), current.permissions());
            }
        }
        crate::file_watcher::persist_tempfile(tmp, &target)
    })();
    if let Err(e) = write_result {
        return (
            dashboard_fs_io_status(&e).to_string(),
            serde_json::json!({
                "error": format!("could not write {}: {e}", target.display())
            }),
        );
    }

    let modified_ms = std::fs::metadata(&target)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(system_time_unix_ms);
    (
        "200 OK".to_string(),
        serde_json::json!({
            "ok": true,
            "path": target.to_string_lossy().to_string(),
            "size": bytes.len(),
            "sha256": fs_sha256_hex(bytes),
            "created": !existed,
            "modified_ms": modified_ms,
        }),
    )
}

/// Rename/move half of the dashboard editor. Same contract as
/// [`apply_dashboard_fs_write`]: the caller has already routed **both** paths
/// through the write-scope gate — this function performs no IAM checks of its
/// own.
///
/// The source must exist; the destination's parent must exist; the
/// destination itself must not (`409` `code:"exists"` — a rename never
/// replaces, even though the underlying syscall would). Cross-filesystem
/// moves are refused rather than silently degraded to copy+delete.
pub(crate) fn apply_dashboard_fs_rename(
    raw_from: &str,
    raw_to: &str,
) -> (String, serde_json::Value) {
    let from = match expand_dashboard_fs_path(raw_from) {
        Ok(path) => path,
        Err(e) => {
            return (
                "400 Bad Request".to_string(),
                serde_json::json!({ "error": e }),
            )
        }
    };
    let from = match std::fs::canonicalize(&from) {
        Ok(canonical) => canonical,
        Err(e) => {
            return (
                dashboard_fs_io_status(&e).to_string(),
                serde_json::json!({
                    "error": format!("{} is not accessible: {e}", from.display()),
                    "code": "missing",
                }),
            )
        }
    };
    let to = match expand_dashboard_fs_path(raw_to) {
        Ok(path) => path,
        Err(e) => {
            return (
                "400 Bad Request".to_string(),
                serde_json::json!({ "error": e }),
            )
        }
    };
    // Resolve the destination the same way a creating write does: canonical
    // parent (which must exist) + final name, so `..` segments and symlinked
    // parents cannot smuggle the target elsewhere after authorization.
    let Some(name) = to.file_name().map(|n| n.to_os_string()) else {
        return (
            "400 Bad Request".to_string(),
            serde_json::json!({
                "error": format!("{} has no file name", to.display())
            }),
        );
    };
    let Some(parent) = to.parent() else {
        return (
            "400 Bad Request".to_string(),
            serde_json::json!({
                "error": format!("{} has no parent directory", to.display())
            }),
        );
    };
    let canonical_parent = match std::fs::canonicalize(parent) {
        Ok(canonical) => canonical,
        Err(_) => {
            return (
                "404 Not Found".to_string(),
                serde_json::json!({
                    "error": format!(
                        "parent directory {} does not exist — create it first",
                        parent.display()
                    ),
                    "code": "missing_parent",
                }),
            )
        }
    };
    if !canonical_parent.is_dir() {
        return (
            "400 Bad Request".to_string(),
            serde_json::json!({
                "error": format!("{} is not a directory", canonical_parent.display())
            }),
        );
    }
    let to = canonical_parent.join(name);
    if to == from {
        return (
            "200 OK".to_string(),
            serde_json::json!({
                "ok": true,
                "from": from.to_string_lossy().to_string(),
                "path": to.to_string_lossy().to_string(),
                "renamed": false,
                "notice": "source and destination are the same path",
            }),
        );
    }
    if from.is_dir() && to.starts_with(&from) {
        return (
            "400 Bad Request".to_string(),
            serde_json::json!({
                "error": format!(
                    "cannot move {} into itself",
                    from.display()
                ),
            }),
        );
    }
    if to.symlink_metadata().is_ok() {
        return (
            "409 Conflict".to_string(),
            serde_json::json!({
                "error": format!("{} already exists", to.display()),
                "code": "exists",
            }),
        );
    }
    match std::fs::rename(&from, &to) {
        Ok(()) => (
            "200 OK".to_string(),
            serde_json::json!({
                "ok": true,
                "from": from.to_string_lossy().to_string(),
                "path": to.to_string_lossy().to_string(),
                "renamed": true,
            }),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => (
            "400 Bad Request".to_string(),
            serde_json::json!({
                "error": format!(
                    "{} and {} are on different filesystems — copy and delete instead",
                    from.display(),
                    to.display()
                ),
                "code": "cross_device",
            }),
        ),
        Err(e) => (
            dashboard_fs_io_status(&e).to_string(),
            serde_json::json!({
                "error": format!(
                    "could not rename {} to {}: {e}",
                    from.display(),
                    to.display()
                ),
            }),
        ),
    }
}

/// Delete half of the dashboard editor. Same contract as
/// [`apply_dashboard_fs_write`]: the caller has already routed the path
/// through the write-scope gate — this function performs no IAM checks of
/// its own.
///
/// Symlinks are deleted as links (`symlink_metadata`, never following), so a
/// link whose target sits outside the caller's scope removes only the link.
/// Non-empty directories require an explicit `recursive` (`409`
/// `code:"not_empty"` otherwise).
pub(crate) fn apply_dashboard_fs_delete(
    raw_path: &str,
    recursive: bool,
) -> (String, serde_json::Value) {
    let path = match expand_dashboard_fs_path(raw_path) {
        Ok(path) => path,
        Err(e) => {
            return (
                "400 Bad Request".to_string(),
                serde_json::json!({ "error": e }),
            )
        }
    };
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(e) => {
            return (
                dashboard_fs_io_status(&e).to_string(),
                serde_json::json!({
                    "error": format!("{} is not accessible: {e}", path.display()),
                    "code": "missing",
                }),
            )
        }
    };
    let is_dir = metadata.is_dir();
    let result = if is_dir {
        if recursive {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_dir(&path)
        }
    } else {
        std::fs::remove_file(&path)
    };
    match result {
        Ok(()) => (
            "200 OK".to_string(),
            serde_json::json!({
                "ok": true,
                "path": path.to_string_lossy().to_string(),
                "deleted": true,
                "dir": is_dir,
            }),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => (
            "409 Conflict".to_string(),
            serde_json::json!({
                "error": format!(
                    "{} is not empty — pass recursive to delete its contents",
                    path.display()
                ),
                "code": "not_empty",
            }),
        ),
        Err(e) => (
            dashboard_fs_io_status(&e).to_string(),
            serde_json::json!({
                "error": format!("could not delete {}: {e}", path.display()),
            }),
        ),
    }
}

pub(crate) fn status_line_u16(status_line: &str) -> u16 {
    status_line
        .split_whitespace()
        .next()
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(500)
}

/// Build an HTTP response for an upload endpoint error.
pub(crate) fn upload_error_response(status: &str, message: &str) -> String {
    let body = serde_json::json!({"error": message}).to_string();
    HttpResponse::with_content(status, "application/json", body)
        .header("Cache-Control", "no-cache")
        .header("Access-Control-Allow-Origin", "*")
        .header("Connection", "close")
        .into_string()
}

/// Transport-neutral core of `POST /api/fs/write` (tunnel twin
/// `api_fs_write`'s apply leg): decode the request's content payload,
/// then run the precondition-guarded atomic write off the async
/// runtime. Path authorization is the caller's lane gate.
pub(crate) async fn fs_write_api_response(req: FsWriteRequest) -> ApiResponse {
    match fs_write_request_bytes(&req) {
        Ok(bytes) => {
            let args = FsWriteArgs {
                path: req.path.clone(),
                expected_sha256: req.expected_sha256.clone(),
                create_new: req.create_new,
                force: req.force,
            };
            fs_write_bytes_api_response(args, bytes).await
        }
        Err(message) => ApiResponse::json_error(400, message),
    }
}

/// Bytes-taking transport-neutral core of the fs write apply leg. The
/// content-carriage asymmetry is preserved by design (§2.7): HTTP
/// decodes JSON `content`/`content_b64` into these bytes
/// ([`fs_write_api_response`] above); the tunnel spools them from
/// `upload_start/chunk/end` frames. Path authorization is the caller's
/// lane gate.
pub(crate) async fn fs_write_bytes_api_response(args: FsWriteArgs, bytes: Vec<u8>) -> ApiResponse {
    let (status, body) =
        tokio::task::spawn_blocking(move || apply_dashboard_fs_write(&args, &bytes))
            .await
            .unwrap_or_else(|e| {
                (
                    "500 Internal Server Error".to_string(),
                    serde_json::json!({
                        "error": format!(
                            "filesystem write task failed: {e}"
                        )
                    }),
                )
            });
    ApiResponse::json(
        status_line_u16(&status),
        JsonBody::PreSerialized(body.to_string()),
    )
}

/// The tunnel's write params→args fold (lifted from the retired
/// `dashboard_fs_write_response_parts` bridge; HTTP's twin decode is
/// the serde [`FsWriteRequest`]). Param decode stays transport-owned:
/// these are the historical lenient reads, verbatim.
pub(crate) fn fs_write_args_from_params(params: &serde_json::Value) -> FsWriteArgs {
    FsWriteArgs {
        path: params
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        expected_sha256: params
            .get("expected_sha256")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        create_new: params
            .get("create_new")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        force: params
            .get("force")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

pub(crate) async fn handle_fs_write(
    stream: DemuxStream,
    body_text: String,
    http_access_context: HttpAccessContext,
    peer_connection_identity: Option<PeerConnectionIdentity>,
    bus: EventBus,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    // Dispatch already read the body under the row's envelope cap
    // (UPLOAD_MAX_BYTES plus half again — base64/escaping overhead on top
    // of the content cap apply_dashboard_fs_write enforces).
    let response = match serde_json::from_str::<FsWriteRequest>(&body_text) {
        Ok(req) => match authorize_http_filesystem_access(
            &http_access_context,
            peer_connection_identity.as_ref(),
            crate::peer::access_policy::PeerOperation::FilesystemWrite,
            crate::peer::access_policy::FilesystemAccessKind::Write,
            &req.path,
            &bus,
        ) {
            Ok(()) => fs_write_api_response(req).await,
            Err(message) => ApiResponse::json_error(403, message),
        },
        Err(e) => ApiResponse::json_error(400, format!("invalid JSON: {e}")),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Synthesize the fs GET trio's [`ApiRequest`] from the HTTP request
/// line: the tunnel's params shape (`{"path": …}`) built from the query
/// string, plus the verbatim `Range` header for reads.
fn fs_api_request(request_line: &str, range: Option<ByteRange>) -> ApiRequest {
    ApiRequest {
        params: serde_json::json!({
            "path": query_param(request_line, "path").unwrap_or_default(),
        }),
        range,
    }
}

/// Transport-neutral core of `GET /api/fs/stat` (tunnel twin
/// `api_fs_stat`; the tunnel lane delegates here in S2).
pub(crate) fn fs_stat_api_response(request: &ApiRequest) -> ApiResponse {
    match inspect_dashboard_fs_path(request.str_param("path")) {
        Ok(status) => ApiResponse::json(
            200,
            JsonBody::PreSerialized(
                serde_json::to_string(&status).unwrap_or_else(|_| "{}".to_string()),
            ),
        ),
        Err(e) => ApiResponse::json_error(400, e),
    }
}

pub(crate) async fn handle_fs_stat(
    stream: DemuxStream,
    request_line: &str,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = fs_stat_api_response(&fs_api_request(request_line, None));
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of `GET /api/fs/list` (tunnel twin
/// `api_fs_list`; the tunnel lane delegates here in S2).
pub(crate) fn fs_list_api_response(request: &ApiRequest) -> ApiResponse {
    match list_dashboard_fs_dir(request.str_param("path")) {
        Ok(body) => ApiResponse::json(200, JsonBody::Value(body)),
        Err(e) => ApiResponse::json_error(400, e),
    }
}

pub(crate) async fn handle_fs_list(
    stream: DemuxStream,
    request_line: &str,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = fs_list_api_response(&fs_api_request(request_line, None));
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of `GET /api/fs/read` (tunnel twin
/// `api_fs_read`): bytes lane on success (200 full / 206 partial), JSON
/// error shapes otherwise (the 416 keeps its range-probing header tail).
/// Each [`ByteRange`] form keeps its transport's historical semantics —
/// the divergences are enumerated on the tunnel-side parity fixtures.
pub(crate) fn fs_read_api_response(request: &ApiRequest) -> ApiResponse {
    let range_header = match &request.range {
        Some(ByteRange::OffsetLength { offset, length }) => {
            return fs_read_offset_length_api_response(request.str_param("path"), *offset, *length);
        }
        Some(ByteRange::HttpHeader(value)) => Some(value.as_str()),
        None => None,
    };
    match dashboard_fs_read_file(request.str_param("path"), range_header) {
        Ok(file) => {
            let mut headers: Vec<(&'static str, String)> =
                vec![("Accept-Ranges", "bytes".to_string())];
            if file.partial {
                headers.push((
                    "Content-Range",
                    format!(
                        "bytes {}-{}/{}",
                        file.range_start,
                        file.range_end.saturating_sub(1),
                        file.total_size
                    ),
                ));
            } else {
                // Full (non-range) reads carry the content
                // hash so the editor has a conflict baseline
                // for its later write-back.
                headers.push(("X-Content-Sha256", fs_sha256_hex(&file.bytes)));
            }
            headers.push((
                "Content-Disposition",
                format!(
                    "attachment; filename=\"{}\"",
                    file.filename.replace('"', "")
                ),
            ));
            headers.push(("Cache-Control", "no-cache".to_string()));
            headers.push(("Access-Control-Allow-Origin", "*".to_string()));
            headers.push((
                "Access-Control-Expose-Headers",
                "X-Content-Sha256".to_string(),
            ));
            headers.push(("Connection", "close".to_string()));
            ApiResponse::Bytes {
                status: if file.partial { 206 } else { 200 },
                content_type: file.content_type,
                headers,
                bytes: BytesPayload::InMemory(file.bytes),
                meta: serde_json::Value::Null,
            }
        }
        Err(error) => {
            let status = status_line_u16(&error.status);
            if let Some(total_size) = error.total_size {
                ApiResponse::Json {
                    status,
                    body: JsonBody::Value(serde_json::json!({ "error": error.message })),
                    headers: vec![
                        ("Content-Range", format!("bytes */{total_size}")),
                        ("Accept-Ranges", "bytes".to_string()),
                        ("Cache-Control", "no-cache".to_string()),
                        ("Access-Control-Allow-Origin", "*".to_string()),
                        ("Connection", "close".to_string()),
                    ],
                }
            } else {
                ApiResponse::json_error(status, error.message)
            }
        }
    }
}

/// The offset/length read form — the datachannel tunnel's historical
/// `api_fs_read` semantics, preserved byte-for-byte through the S2
/// delegation: errors keep their `{"ok": false, …}` bodies and their
/// wordings; success carries the payload plus the tunnel's result
/// object as [`ApiResponse::Bytes`] `meta` (filename and content type
/// derive from the expanded — not canonicalized — path, exactly as
/// before). Status and headers on the success arm are HTTP-lane
/// decoration only until the transfer rows (S9) define this form's
/// HTTP rendering; the tunnel framer consumes `content_type`, `bytes`,
/// and `meta`.
fn fs_read_offset_length_api_response(
    raw_path: &str,
    offset: u64,
    length: Option<u64>,
) -> ApiResponse {
    let path = match expand_dashboard_fs_path(raw_path) {
        Ok(path) => path,
        Err(error) => {
            return ApiResponse::json(
                400,
                JsonBody::Value(serde_json::json!({ "ok": false, "error": error })),
            );
        }
    };
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty());
    let content_type = dashboard_fs_content_type(&path);
    let (bytes, total_size, end, display_path) =
        match read_dashboard_fs_file_range(&path, offset, length) {
            Ok(value) => value,
            Err((status, body)) => return ApiResponse::json(status, JsonBody::Value(body)),
        };
    // Full-extent reads carry the content hash so the editor has a
    // conflict baseline for its later write-back (mirrors the HTTP
    // route's X-Content-Sha256 header; this form is extent-based where
    // the header form keys off the request shape).
    let full = offset == 0 && bytes.len() as u64 == total_size;
    let sha256 = full.then(|| fs_sha256_hex(&bytes));
    let meta = serde_json::json!({
        "ok": true,
        "path": display_path.to_string_lossy().to_string(),
        "filename": filename,
        "content_type": content_type.clone(),
        "size": bytes.len(),
        "total_size": total_size,
        "offset": offset,
        "range_start": offset,
        "range_end": end,
        "resumable": true,
        "sha256": sha256,
    });
    ApiResponse::Bytes {
        status: if full { 200 } else { 206 },
        content_type,
        headers: vec![
            ("Cache-Control", "no-cache".to_string()),
            ("Connection", "close".to_string()),
        ],
        bytes: BytesPayload::InMemory(bytes),
        meta,
    }
}

pub(crate) fn read_dashboard_fs_file_range(
    path: &Path,
    offset: u64,
    length: Option<u64>,
) -> Result<(Vec<u8>, u64, u64, PathBuf), (u16, serde_json::Value)> {
    use std::io::{Read as _, Seek as _};
    let metadata = std::fs::metadata(path).map_err(|e| {
        (
            404,
            serde_json::json!({ "ok": false, "error": format!("file not accessible: {e}") }),
        )
    })?;
    if !metadata.is_file() {
        return Err((
            400,
            serde_json::json!({ "ok": false, "error": "path is not a regular file" }),
        ));
    }
    let total_size = metadata.len();
    let (start, transfer_len, end) = filesystem_read_range(total_size, offset, length)?;
    let mut file = std::fs::File::open(path).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("open file: {e}") }),
        )
    })?;
    file.seek(std::io::SeekFrom::Start(start)).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("seek file: {e}") }),
        )
    })?;
    let mut bytes = vec![0u8; transfer_len];
    file.read_exact(&mut bytes).map_err(|e| {
        (
            500,
            serde_json::json!({ "ok": false, "error": format!("read file: {e}") }),
        )
    })?;
    let display = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Ok((bytes, total_size, end, display))
}

pub(crate) fn filesystem_read_range(
    total_size: u64,
    offset: u64,
    length: Option<u64>,
) -> Result<(u64, usize, u64), (u16, serde_json::Value)> {
    if offset > total_size {
        return Err((
            416,
            serde_json::json!({
                "ok": false,
                "error": "range start beyond file size",
                "total_size": total_size,
            }),
        ));
    }
    let available = total_size.saturating_sub(offset);
    let requested = length.unwrap_or(available).min(available);
    if requested > crate::web_gateway::UPLOAD_MAX_BYTES as u64 {
        return Err((
            413,
            serde_json::json!({
                "ok": false,
                "error": format!(
                    "range too large: {} bytes (cap is {})",
                    requested,
                    crate::web_gateway::UPLOAD_MAX_BYTES
                ),
            }),
        ));
    }
    let transfer_len = usize::try_from(requested).map_err(|_| {
        (
            413,
            serde_json::json!({ "ok": false, "error": "range too large for this platform" }),
        )
    })?;
    Ok((offset, transfer_len, offset.saturating_add(requested)))
}

pub(crate) async fn handle_fs_read(
    stream: DemuxStream,
    header_text: &str,
    request_line: &str,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let range = dashboard_http_header_value(header_text, "range")
        .map(|value| ByteRange::HttpHeader(value.to_string()));
    let response = fs_read_api_response(&fs_api_request(request_line, range));
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of `POST /api/fs/mkdir` (tunnel twin
/// `api_fs_mkdir`; the tunnel lane delegates here in S2). Path
/// authorization is the caller's lane gate.
pub(crate) fn fs_mkdir_api_response(path: &str) -> ApiResponse {
    match mkdir_dashboard_fs_path(path) {
        Ok(body) => ApiResponse::json(200, JsonBody::Value(body)),
        Err((status, message)) => ApiResponse::json_error(status_line_u16(&status), message),
    }
}

pub(crate) async fn handle_fs_mkdir(
    stream: DemuxStream,
    body_text: String,
    http_access_context: HttpAccessContext,
    peer_connection_identity: Option<PeerConnectionIdentity>,
    bus: EventBus,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = match serde_json::from_str::<FsMkdirRequest>(&body_text) {
        Ok(req) => match authorize_http_filesystem_access(
            &http_access_context,
            peer_connection_identity.as_ref(),
            crate::peer::access_policy::PeerOperation::FilesystemWrite,
            crate::peer::access_policy::FilesystemAccessKind::Write,
            &req.path,
            &bus,
        ) {
            Ok(()) => fs_mkdir_api_response(&req.path),
            Err(message) => ApiResponse::json_error(403, message),
        },
        Err(e) => ApiResponse::json_error(400, format!("invalid JSON: {e}")),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of `POST /api/fs/rename` (tunnel twin
/// `api_fs_rename`; the tunnel lane delegates here in S2). Both paths'
/// authorization is the caller's lane gate.
pub(crate) async fn fs_rename_api_response(from: String, to: String) -> ApiResponse {
    let (status, body) =
        tokio::task::spawn_blocking(move || apply_dashboard_fs_rename(&from, &to))
            .await
            .unwrap_or_else(|e| {
                (
                    "500 Internal Server Error".to_string(),
                    serde_json::json!({
                        "error": format!(
                            "filesystem rename task failed: {e}"
                        )
                    }),
                )
            });
    ApiResponse::json(
        status_line_u16(&status),
        JsonBody::PreSerialized(body.to_string()),
    )
}

pub(crate) async fn handle_fs_rename(
    stream: DemuxStream,
    body_text: String,
    http_access_context: HttpAccessContext,
    peer_connection_identity: Option<PeerConnectionIdentity>,
    bus: EventBus,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = match serde_json::from_str::<FsRenameRequest>(&body_text) {
        // Removing the source entry and creating the
        // destination are both writes — each leg passes
        // the write-scope gate on its own path.
        Ok(req) => match authorize_http_filesystem_access(
            &http_access_context,
            peer_connection_identity.as_ref(),
            crate::peer::access_policy::PeerOperation::FilesystemWrite,
            crate::peer::access_policy::FilesystemAccessKind::Write,
            &req.from,
            &bus,
        )
        .and_then(|()| {
            authorize_http_filesystem_access(
                &http_access_context,
                peer_connection_identity.as_ref(),
                crate::peer::access_policy::PeerOperation::FilesystemWrite,
                crate::peer::access_policy::FilesystemAccessKind::Write,
                &req.to,
                &bus,
            )
        }) {
            Ok(()) => fs_rename_api_response(req.from, req.to).await,
            Err(message) => ApiResponse::json_error(403, message),
        },
        Err(e) => ApiResponse::json_error(400, format!("invalid JSON: {e}")),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

/// Transport-neutral core of `POST /api/fs/delete` (tunnel twin
/// `api_fs_delete`; the tunnel lane delegates here in S2). Path
/// authorization is the caller's lane gate.
pub(crate) async fn fs_delete_api_response(path: String, recursive: bool) -> ApiResponse {
    let (status, body) =
        tokio::task::spawn_blocking(move || apply_dashboard_fs_delete(&path, recursive))
            .await
            .unwrap_or_else(|e| {
                (
                    "500 Internal Server Error".to_string(),
                    serde_json::json!({
                        "error": format!(
                            "filesystem delete task failed: {e}"
                        )
                    }),
                )
            });
    ApiResponse::json(
        status_line_u16(&status),
        JsonBody::PreSerialized(body.to_string()),
    )
}

pub(crate) async fn handle_fs_delete(
    stream: DemuxStream,
    body_text: String,
    http_access_context: HttpAccessContext,
    peer_connection_identity: Option<PeerConnectionIdentity>,
    bus: EventBus,
    cors: crate::gateway_routes::CorsPosture,
    fleet_origin: Option<&str>,
) {
    let response = match serde_json::from_str::<FsDeleteRequest>(&body_text) {
        Ok(req) => match authorize_http_filesystem_access(
            &http_access_context,
            peer_connection_identity.as_ref(),
            crate::peer::access_policy::PeerOperation::FilesystemWrite,
            crate::peer::access_policy::FilesystemAccessKind::Write,
            &req.path,
            &bus,
        ) {
            Ok(()) => fs_delete_api_response(req.path, req.recursive).await,
            Err(message) => ApiResponse::json_error(403, message),
        },
        Err(e) => ApiResponse::json_error(400, format!("invalid JSON: {e}")),
    };
    write_api_response(stream, response, cors, fleet_origin).await;
}

// Parameter count rides until a request-context bundle collapses the
// shared per-connection arguments (open cleanup; not load-bearing).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle_current_uploads_post(
    mut stream: DemuxStream,
    header_text: &str,
    request_line: &str,
    discard: Vec<u8>,
    bus: EventBus,
    project_root_for_changes: Option<PathBuf>,
    session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
    daemon_session_id: Option<String>,
) {
    // POST /api/session/current/uploads?name=<fn>&destination=task|workspace
    //   Content-Type: <mime>
    //   <raw bytes>
    //
    // Streams the body into a tempfile, commits it into
    // the upload store for this daemon's scope (the
    // project-local ignored `.intendant/uploads/<session-id>/`,
    // or the daemon-global store on projectless daemons),
    // and broadcasts UploadReady so all connected
    // browsers see it.
    //
    // Route sits in the `/api/session/current/*` family
    // alongside `changes`, `history`, `rollback`, etc.
    // That namespace is browser-session managed — not
    // part of `is_federation_path`, so bearer-token auth
    // doesn't apply. If a WAN-exposed deploy wants to
    // protect uploads, gate the whole family at once.
    use tokio::io::AsyncWriteExt;
    let response = 'upload: {
        let scope = crate::global_store::StoreScope::resolve(project_root_for_changes.as_deref());

        let name = query_param(request_line, "name").unwrap_or_else(|| "upload.bin".to_string());
        let requested_destination = query_param(request_line, "destination")
            .as_deref()
            .and_then(crate::upload_store::UploadDestination::from_str)
            .unwrap_or(crate::upload_store::UploadDestination::Task);
        let mime = content_type_header(header_text);
        if header_text
            .lines()
            .any(|l| l.trim().eq_ignore_ascii_case("expect: 100-continue"))
        {
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await;
        }

        match stream_body_to_tempfile(header_text, &discard, &mut stream, UPLOAD_MAX_BYTES).await {
            Err(e) => {
                let status = if e.contains("too large") {
                    "413 Payload Too Large"
                } else {
                    "400 Bad Request"
                };
                break 'upload upload_error_response(status, &e);
            }
            Ok((tmp, size)) => {
                let (session_dir, session_id) = {
                    if let Some(ref slog) = session_log {
                        match slog.lock() {
                            Ok(l) => (l.dir().to_path_buf(), l.session_id().to_string()),
                            Err(_) => {
                                break 'upload upload_error_response(
                                    "500 Internal Server Error",
                                    "session log lock poisoned",
                                );
                            }
                        }
                    } else {
                        (
                            pending_upload_session_dir(&scope),
                            daemon_session_id
                                .clone()
                                .unwrap_or_else(|| "pending".to_string()),
                        )
                    }
                };
                let destination =
                    effective_upload_destination(requested_destination, session_log.is_some());
                match crate::upload_store::commit_upload(
                    tmp,
                    &name,
                    &mime,
                    size as u64,
                    destination,
                    &session_dir,
                    &session_id,
                    &scope,
                ) {
                    Ok(descriptor) => {
                        bus.send(crate::event::AppEvent::UploadReady {
                            descriptor: descriptor.clone(),
                        });
                        let body =
                            serde_json::to_string(&descriptor).unwrap_or_else(|_| "{}".to_string());
                        HttpResponse::with_content("200 OK", "application/json", body)
                            .header("Cache-Control", "no-cache")
                            .header("Access-Control-Allow-Origin", "*")
                            .header("Connection", "close")
                            .into_string()
                    }
                    Err(e) => upload_error_response(
                        "500 Internal Server Error",
                        &format!("commit upload: {e}"),
                    ),
                }
            }
        }
    };
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

pub(crate) async fn handle_current_uploads_get(
    mut stream: DemuxStream,
    request_line: &str,
    project_root_for_changes: Option<PathBuf>,
    session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
) {
    // GET /api/session/current/uploads           — list uploads for the current session
    // GET /api/session/current/uploads/<id>/raw  — stream bytes of one upload
    use tokio::io::AsyncWriteExt;
    let response = 'get_upload: {
        let scope = crate::global_store::StoreScope::resolve(project_root_for_changes.as_deref());
        let session_dir = if let Some(ref slog) = session_log {
            match slog.lock() {
                Ok(l) => l.dir().to_path_buf(),
                Err(_) => {
                    break 'get_upload upload_error_response(
                        "500 Internal Server Error",
                        "session log lock poisoned",
                    );
                }
            }
        } else {
            pending_upload_session_dir(&scope)
        };
        // Path after /api/session/current/uploads
        let path_and_q = request_line.split_whitespace().nth(1).unwrap_or("");
        let path = path_and_q.split('?').next().unwrap_or("");
        let suffix = path
            .trim_start_matches("/api/session/current/uploads")
            .trim_matches('/');
        if suffix.is_empty() {
            let uploads = crate::upload_store::list_uploads(&session_dir, &scope);
            let body = serde_json::to_string(&uploads).unwrap_or_else(|_| "[]".to_string());
            HttpResponse::with_content("200 OK", "application/json", body)
                .header("Cache-Control", "no-cache")
                .header("Access-Control-Allow-Origin", "*")
                .header("Connection", "close")
                .into_string()
        } else if let Some(id) = suffix.strip_suffix("/raw") {
            // GET raw bytes for one upload.
            match crate::upload_store::find_upload(id, &session_dir, &scope) {
                None => upload_error_response("404 Not Found", "upload not found"),
                Some(d) => {
                    match std::fs::read(&d.path) {
                        Ok(bytes) => {
                            let header = HttpResponse::new("200 OK")
                                .header("Content-Type", d.mime)
                                .header("Content-Length", bytes.len().to_string())
                                .header(
                                    "Content-Disposition",
                                    format!("inline; filename=\"{}\"", d.name.replace('"', ""),),
                                )
                                .header("Cache-Control", "no-cache")
                                .header("Access-Control-Allow-Origin", "*")
                                .header("Connection", "close")
                                .into_string();
                            let _ = stream.write_all(header.as_bytes()).await;
                            let _ = stream.write_all(&bytes).await;
                            // Skip the trailing write_all below.
                            break 'get_upload String::new();
                        }
                        Err(e) => upload_error_response(
                            "500 Internal Server Error",
                            &format!("read upload: {e}"),
                        ),
                    }
                }
            }
        } else {
            upload_error_response("404 Not Found", "unknown upload route")
        }
    };
    if !response.is_empty() {
        let _ = stream.write_all(response.as_bytes()).await;
    }
    finalize_http_stream(&mut stream).await;
}

pub(crate) async fn handle_current_upload_delete(
    mut stream: DemuxStream,
    request_line: &str,
    bus: EventBus,
    project_root_for_changes: Option<PathBuf>,
    session_log: Option<Arc<Mutex<crate::session_log::SessionLog>>>,
) {
    // DELETE /api/session/current/uploads/<id> — remove the file + sidecar.
    use tokio::io::AsyncWriteExt;
    let response = {
        let session_dir = if let Some(ref slog) = session_log {
            match slog.lock() {
                Ok(l) => Ok(Some(l.dir().to_path_buf())),
                Err(_) => Err("session log lock poisoned"),
            }
        } else {
            Ok(None)
        };
        match session_dir {
            Err(error) => json_response(
                "500 Internal Server Error",
                serde_json::json!({ "error": error }).to_string(),
            ),
            Ok(session_dir) => {
                let path_and_q = request_line.split_whitespace().nth(1).unwrap_or("");
                let path = path_and_q.split('?').next().unwrap_or("");
                let id = path
                    .trim_start_matches("/api/session/current/uploads/")
                    .trim_matches('/');
                let (status, body, deleted_id) = current_upload_delete_response_body(
                    project_root_for_changes.as_deref(),
                    session_dir.as_deref(),
                    id,
                );
                if let Some(id) = deleted_id {
                    bus.send(crate::event::AppEvent::UploadDeleted { id });
                }
                json_response(status, body)
            }
        }
    };
    let _ = stream.write_all(response.as_bytes()).await;
    finalize_http_stream(&mut stream).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_viewer_request_strips_line_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let file = src_dir.join("file name.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let encoded_path = file.to_string_lossy().replace(' ', "%20");
        let request_line = format!("GET {encoded_path}:42 HTTP/1.1");

        let parsed = dashboard_source_request_from_line(&request_line).unwrap();
        assert_eq!(parsed.path, file);
        assert_eq!(parsed.line, Some(42));
    }

    #[test]
    fn source_viewer_request_ignores_dashboard_routes() {
        assert!(dashboard_source_request_from_line("GET /sessions HTTP/1.1").is_none());
    }

    #[test]
    fn source_viewer_response_embeds_file_and_target_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "fn one() {}\nfn two() {}\n").unwrap();
        let request_line = format!("GET {}:2 HTTP/1.1", file.to_string_lossy());

        let (status, body) = dashboard_source_viewer_response(&request_line).unwrap();
        assert_eq!(status, "200 OK");
        assert!(body.contains("\"line\":2"), "{body}");
        assert!(body.contains("\"language\":\"rust\""), "{body}");
        assert!(body.contains("source-code"), "{body}");
    }

    #[test]
    fn dashboard_local_file_response_serves_image_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("screen shot.png");
        let image_bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 1, 2, 3];
        std::fs::write(&file, &image_bytes).unwrap();
        let encoded_path = file.to_string_lossy().replace(' ', "%20");
        let request_line = format!("GET {encoded_path} HTTP/1.1");

        match dashboard_local_file_response(&request_line).unwrap() {
            DashboardLocalFileResponse::Bytes {
                status,
                content_type,
                bytes,
            } => {
                assert_eq!(status, "200 OK");
                assert_eq!(content_type, "image/png");
                assert_eq!(bytes, image_bytes);
            }
            DashboardLocalFileResponse::Html { body, .. } => {
                panic!("expected image bytes, got html: {body}")
            }
        }
    }

    #[test]
    fn dashboard_local_file_response_keeps_text_source_viewer() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "fn one() {}\nfn two() {}\n").unwrap();
        let request_line = format!("GET {}:2 HTTP/1.1", file.to_string_lossy());

        match dashboard_local_file_response(&request_line).unwrap() {
            DashboardLocalFileResponse::Html { status, body } => {
                assert_eq!(status, "200 OK");
                assert!(body.contains("\"line\":2"), "{body}");
                assert!(body.contains("source-code"), "{body}");
            }
            DashboardLocalFileResponse::Bytes { .. } => {
                panic!("expected source viewer html, got bytes")
            }
        }
    }

    #[test]
    fn pending_upload_session_dir_follows_store_scope() {
        let root = std::path::PathBuf::from("/tmp/project");
        assert_eq!(
            pending_upload_session_dir(&crate::global_store::StoreScope::Project(root.clone())),
            root.join(".intendant").join("pending_uploads")
        );
        let global = std::path::PathBuf::from("/tmp/state/global-store");
        assert_eq!(
            pending_upload_session_dir(&crate::global_store::StoreScope::Global(global.clone())),
            global.join("pending_uploads")
        );
    }

    #[test]
    fn dashboard_fs_stat_reports_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let status = inspect_dashboard_fs_path(dir.path().to_str().unwrap()).unwrap();

        assert!(status.exists);
        assert!(status.is_dir);
        assert!(status.readable);
        assert!(!status.can_create);
    }

    #[test]
    fn dashboard_fs_stat_marks_missing_directory_creatable() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("new").join("project");
        let status = inspect_dashboard_fs_path(missing.to_str().unwrap()).unwrap();

        assert!(!status.exists);
        assert!(status.can_create);
        assert_eq!(
            status.nearest_existing_parent.as_deref(),
            Some(dir.path().to_str().unwrap())
        );
    }

    #[test]
    fn dashboard_fs_mkdir_creates_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("new").join("project");
        let result = mkdir_dashboard_fs_path(missing.to_str().unwrap()).unwrap();

        assert_eq!(result["created"], true);
        assert_eq!(result["already_exists"], false);
        assert!(missing.is_dir());
    }

    #[test]
    fn dashboard_fs_mkdir_reports_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let result = mkdir_dashboard_fs_path(dir.path().to_str().unwrap()).unwrap();

        assert_eq!(result["created"], false);
        assert_eq!(result["already_exists"], true);
    }

    #[test]
    fn dashboard_fs_read_file_returns_attachment_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        std::fs::write(&file, b"hello over mtls").unwrap();

        let result = dashboard_fs_read_file(file.to_str().unwrap(), None).unwrap();

        assert_eq!(result.filename, "hello.txt");
        assert_eq!(result.content_type, "text/plain; charset=utf-8");
        assert_eq!(result.total_size, 15);
        assert_eq!(result.range_start, 0);
        assert_eq!(result.range_end, 15);
        assert!(!result.partial);
        assert_eq!(result.bytes, b"hello over mtls");
    }

    #[test]
    fn dashboard_fs_read_file_returns_requested_byte_range() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        std::fs::write(&file, b"hello over mtls").unwrap();

        let result = dashboard_fs_read_file(file.to_str().unwrap(), Some("bytes=6-9")).unwrap();

        assert_eq!(result.total_size, 15);
        assert_eq!(result.range_start, 6);
        assert_eq!(result.range_end, 10);
        assert!(result.partial);
        assert_eq!(result.bytes, b"over");
    }

    #[test]
    fn dashboard_fs_read_file_rejects_unsatisfiable_range() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        std::fs::write(&file, b"hello").unwrap();

        let err = dashboard_fs_read_file(file.to_str().unwrap(), Some("bytes=50-60")).unwrap_err();

        assert_eq!(err.status, "416 Range Not Satisfiable");
        assert_eq!(err.total_size, Some(5));
    }

    #[test]
    fn dashboard_fs_read_file_rejects_directories() {
        let dir = tempfile::tempdir().unwrap();
        let err = dashboard_fs_read_file(dir.path().to_str().unwrap(), None).unwrap_err();

        assert_eq!(err.status, "400 Bad Request");
        assert!(err.message.contains("is not a file"));
    }

    #[test]
    fn fs_write_request_bytes_requires_exactly_one_content_field() {
        let parse = |body: serde_json::Value| {
            fs_write_request_bytes(&serde_json::from_value::<FsWriteRequest>(body).unwrap())
        };
        assert_eq!(
            parse(serde_json::json!({ "path": "/x", "content": "hi" })).unwrap(),
            b"hi"
        );
        assert_eq!(
            parse(serde_json::json!({ "path": "/x", "content_b64": "aGk=" })).unwrap(),
            b"hi"
        );
        assert!(parse(serde_json::json!({ "path": "/x" }))
            .unwrap_err()
            .contains("missing content"));
        assert!(
            parse(serde_json::json!({ "path": "/x", "content": "a", "content_b64": "YQ==" }))
                .unwrap_err()
                .contains("not both")
        );
        assert!(
            parse(serde_json::json!({ "path": "/x", "content_b64": "!!!" }))
                .unwrap_err()
                .contains("not valid base64")
        );
    }

    #[test]
    fn apply_dashboard_fs_write_preconditions_and_atomics() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("app.conf");
        let write = |path: &std::path::Path,
                     bytes: &[u8],
                     expected: Option<String>,
                     create_new: bool,
                     force: bool| {
            apply_dashboard_fs_write(
                &FsWriteArgs {
                    path: path.to_string_lossy().to_string(),
                    expected_sha256: expected,
                    create_new,
                    force,
                },
                bytes,
            )
        };

        // A write with no stated precondition is refused.
        let (status, body) = write(&target, b"v1", None, false, false);
        assert_eq!(status, "400 Bad Request");
        assert_eq!(body["code"], "precondition_required");
        assert!(!target.exists());

        // create_new creates, and refuses to run twice.
        let (status, body) = write(&target, b"v1", None, true, false);
        assert_eq!(status, "200 OK");
        assert_eq!(body["created"], true);
        assert_eq!(body["sha256"].as_str(), Some(fs_sha256_hex(b"v1").as_str()));
        assert_eq!(std::fs::read(&target).unwrap(), b"v1");
        let (status, body) = write(&target, b"v1", None, true, false);
        assert_eq!(status, "409 Conflict");
        assert_eq!(body["code"], "exists");

        // The matching baseline replaces content; a stale one conflicts and
        // reports the current hash without touching the file.
        let (status, body) = write(&target, b"v2", Some(fs_sha256_hex(b"v1")), false, false);
        assert_eq!(status, "200 OK");
        assert_eq!(body["created"], false);
        assert_eq!(std::fs::read(&target).unwrap(), b"v2");
        let (status, body) = write(&target, b"v3", Some(fs_sha256_hex(b"v1")), false, false);
        assert_eq!(status, "409 Conflict");
        assert_eq!(body["code"], "conflict");
        assert_eq!(
            body["current_sha256"].as_str(),
            Some(fs_sha256_hex(b"v2").as_str())
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"v2");

        // force overwrites unconditionally; a baseline against a vanished
        // file reports code:"missing".
        let (status, _) = write(&target, b"v4", None, false, true);
        assert_eq!(status, "200 OK");
        assert_eq!(std::fs::read(&target).unwrap(), b"v4");
        let gone = dir.path().join("gone.conf");
        let (status, body) = write(&gone, b"x", Some(fs_sha256_hex(b"x")), false, false);
        assert_eq!(status, "409 Conflict");
        assert_eq!(body["code"], "missing");

        // Relative paths, directory targets, and missing parents are refused.
        let (status, _) = apply_dashboard_fs_write(
            &FsWriteArgs {
                path: "relative/path".to_string(),
                expected_sha256: None,
                create_new: true,
                force: false,
            },
            b"x",
        );
        assert_eq!(status, "400 Bad Request");
        let (status, _) = write(dir.path(), b"x", None, false, true);
        assert_eq!(status, "400 Bad Request");
        let orphan = dir.path().join("no-such-dir").join("file.txt");
        let (status, body) = write(&orphan, b"x", None, true, false);
        assert_eq!(status, "404 Not Found");
        assert_eq!(body["code"], "missing_parent");

        // Oversized payloads are refused before any disk IO.
        let huge = vec![0u8; UPLOAD_MAX_BYTES + 1];
        let (status, _) = write(&target, &huge, None, false, true);
        assert_eq!(status, "413 Payload Too Large");
        assert_eq!(std::fs::read(&target).unwrap(), b"v4");
    }

    #[test]
    fn apply_dashboard_fs_rename_moves_and_never_replaces() {
        let dir = tempfile::TempDir::new().unwrap();
        let rename = |from: &std::path::Path, to: &std::path::Path| {
            apply_dashboard_fs_rename(&from.to_string_lossy(), &to.to_string_lossy())
        };

        // A plain file rename moves the content.
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, b"payload").unwrap();
        let (status, body) = rename(&a, &b);
        assert_eq!(status, "200 OK", "{body}");
        assert_eq!(body["renamed"], true);
        assert!(!a.exists());
        assert_eq!(std::fs::read(&b).unwrap(), b"payload");

        // A missing source is 404 code:"missing".
        let (status, body) = rename(&a, &dir.path().join("c.txt"));
        assert_eq!(status, "404 Not Found");
        assert_eq!(body["code"], "missing");

        // An existing destination is refused — renames never replace.
        let c = dir.path().join("c.txt");
        std::fs::write(&c, b"other").unwrap();
        let (status, body) = rename(&b, &c);
        assert_eq!(status, "409 Conflict");
        assert_eq!(body["code"], "exists");
        assert_eq!(std::fs::read(&b).unwrap(), b"payload");
        assert_eq!(std::fs::read(&c).unwrap(), b"other");

        // ... even when the destination is a dangling symlink.
        #[cfg(unix)]
        {
            let dangling = dir.path().join("dangling");
            std::os::unix::fs::symlink(dir.path().join("nowhere"), &dangling).unwrap();
            let (status, body) = rename(&b, &dangling);
            assert_eq!(status, "409 Conflict");
            assert_eq!(body["code"], "exists");
        }

        // A missing destination parent is 404 code:"missing_parent".
        let orphan = dir.path().join("no-such-dir").join("b.txt");
        let (status, body) = rename(&b, &orphan);
        assert_eq!(status, "404 Not Found");
        assert_eq!(body["code"], "missing_parent");

        // Directories move too — but never into themselves.
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("inner.txt"), b"x").unwrap();
        let moved = dir.path().join("moved");
        let (status, _) = rename(&sub, &moved);
        assert_eq!(status, "200 OK");
        assert_eq!(std::fs::read(moved.join("inner.txt")).unwrap(), b"x");
        let inside = moved.join("nested");
        let (status, body) = rename(&moved, &inside);
        assert_eq!(status, "400 Bad Request");
        assert!(body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("into itself"));

        // Renaming a path to itself is an explicit no-op.
        let (status, body) = rename(&b, &b);
        assert_eq!(status, "200 OK");
        assert_eq!(body["renamed"], false);

        // Relative paths are refused before touching the filesystem.
        let (status, _) = apply_dashboard_fs_rename("relative/a", "relative/b");
        assert_eq!(status, "400 Bad Request");
    }

    #[test]
    fn apply_dashboard_fs_delete_scopes_directories_and_symlinks() {
        let dir = tempfile::TempDir::new().unwrap();

        // Files delete unconditionally.
        let file = dir.path().join("gone.txt");
        std::fs::write(&file, b"x").unwrap();
        let (status, body) = apply_dashboard_fs_delete(&file.to_string_lossy(), false);
        assert_eq!(status, "200 OK", "{body}");
        assert_eq!(body["dir"], false);
        assert!(!file.exists());

        // Deleting it again is 404 code:"missing".
        let (status, body) = apply_dashboard_fs_delete(&file.to_string_lossy(), false);
        assert_eq!(status, "404 Not Found");
        assert_eq!(body["code"], "missing");

        // Non-empty directories require an explicit recursive.
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("inner.txt"), b"x").unwrap();
        let (status, body) = apply_dashboard_fs_delete(&sub.to_string_lossy(), false);
        assert_eq!(status, "409 Conflict");
        assert_eq!(body["code"], "not_empty");
        assert!(sub.exists());
        let (status, body) = apply_dashboard_fs_delete(&sub.to_string_lossy(), true);
        assert_eq!(status, "200 OK");
        assert_eq!(body["dir"], true);
        assert!(!sub.exists());

        // Empty directories delete without recursive.
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let (status, _) = apply_dashboard_fs_delete(&empty.to_string_lossy(), false);
        assert_eq!(status, "200 OK");
        assert!(!empty.exists());

        // A symlink deletes as a link: the target survives.
        #[cfg(unix)]
        {
            let target = dir.path().join("kept.txt");
            std::fs::write(&target, b"keep me").unwrap();
            let link = dir.path().join("link");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let (status, body) = apply_dashboard_fs_delete(&link.to_string_lossy(), false);
            assert_eq!(status, "200 OK", "{body}");
            assert!(link.symlink_metadata().is_err());
            assert_eq!(std::fs::read(&target).unwrap(), b"keep me");
        }

        // Relative paths are refused before touching the filesystem.
        let (status, _) = apply_dashboard_fs_delete("relative/path", true);
        assert_eq!(status, "400 Bad Request");
    }

    #[cfg(unix)]
    #[test]
    fn apply_dashboard_fs_write_preserves_unix_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("script.sh");
        std::fs::write(&target, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (status, _) = apply_dashboard_fs_write(
            &FsWriteArgs {
                path: target.to_string_lossy().to_string(),
                expected_sha256: Some(fs_sha256_hex(b"#!/bin/sh\n")),
                create_new: false,
                force: false,
            },
            b"#!/bin/sh\necho updated\n",
        );
        assert_eq!(status, "200 OK");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"#!/bin/sh\necho updated\n"
        );
    }

    #[test]
    fn fs_stat_reports_size_and_mtime() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("sized.txt");
        std::fs::write(&file, b"12345").unwrap();
        let status = inspect_dashboard_fs_path(&file.to_string_lossy()).unwrap();
        assert_eq!(status.size, Some(5));
        assert!(status.modified_ms.unwrap_or(0) > 0);
        let missing =
            inspect_dashboard_fs_path(&dir.path().join("nope").to_string_lossy()).unwrap();
        assert_eq!(missing.size, None);
        assert_eq!(missing.modified_ms, None);
    }

    #[test]
    fn upload_destination_is_not_rewritten_without_active_session() {
        assert_eq!(
            effective_upload_destination(crate::upload_store::UploadDestination::Task, false,),
            crate::upload_store::UploadDestination::Task
        );
        assert_eq!(
            effective_upload_destination(crate::upload_store::UploadDestination::Workspace, false,),
            crate::upload_store::UploadDestination::Workspace
        );
        assert_eq!(
            effective_upload_destination(crate::upload_store::UploadDestination::Task, true,),
            crate::upload_store::UploadDestination::Task
        );
    }

    // ── Golden HTTP transcripts: the fs stat/list/read wire contract ──
    //
    // Byte-exact pins of the fs GET trio's HTTP responses, captured
    // before the transport-neutral conversion (transport-unification
    // design §6 S1, risk R1) and kept as the conversion's proof. The
    // expected framing is hand-written below — never built through the
    // response helpers under test.

    /// Run one stream-consuming handler and collect every byte it wrote.
    async fn collect_handler_response<Fut>(run: impl FnOnce(DemuxStream) -> Fut) -> Vec<u8>
    where
        Fut: std::future::Future<Output = ()>,
    {
        use tokio::io::AsyncReadExt;
        let (mut client, server) = tokio::io::duplex(1 << 20);
        run(Box::pin(server)).await;
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("collect handler response");
        response
    }

    /// The historical `json_response` framing, spelled out literally.
    fn golden_json_transcript(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// The CORS posture dispatch hands the shim — read from the route
    /// table so a row-posture change fails these byte pins instead of
    /// silently changing the wire.
    fn fs_route_cors(path: &str) -> crate::gateway_routes::CorsPosture {
        crate::gateway_routes::match_route("GET", path)
            .or_else(|| crate::gateway_routes::match_route("POST", path))
            .expect("fs route declared")
            .0
            .cors
    }

    fn transcript(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[tokio::test]
    async fn golden_fs_stat_success_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        let request_line = format!("GET /api/fs/stat?path={path} HTTP/1.1");
        let response =
            collect_handler_response(|stream| {
            handle_fs_stat(stream, &request_line, fs_route_cors("/api/fs/stat"), None)
        })
        .await;
        let body = serde_json::to_string(&inspect_dashboard_fs_path(&path).unwrap()).unwrap();
        assert_eq!(transcript(&response), golden_json_transcript("200 OK", &body));
    }

    #[tokio::test]
    async fn golden_fs_stat_error_transcript() {
        let request_line = "GET /api/fs/stat?path=relative/notes.txt HTTP/1.1";
        let response =
            collect_handler_response(|stream| {
            handle_fs_stat(stream, request_line, fs_route_cors("/api/fs/stat"), None)
        })
        .await;
        let body = r#"{"error":"path must be absolute or start with ~/ (got relative/notes.txt)"}"#;
        assert_eq!(
            transcript(&response),
            golden_json_transcript("400 Bad Request", body)
        );
    }

    #[tokio::test]
    async fn golden_fs_list_success_transcript() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("beta")).unwrap();
        std::fs::write(dir.path().join("alpha.txt"), b"a").unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        let request_line = format!("GET /api/fs/list?path={path} HTTP/1.1");
        let response =
            collect_handler_response(|stream| {
            handle_fs_list(stream, &request_line, fs_route_cors("/api/fs/list"), None)
        })
        .await;
        let body = list_dashboard_fs_dir(&path).unwrap().to_string();
        assert_eq!(transcript(&response), golden_json_transcript("200 OK", &body));
    }

    #[tokio::test]
    async fn golden_fs_list_error_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plain.txt");
        std::fs::write(&file, b"not a directory").unwrap();
        let path = file.to_string_lossy().into_owned();
        let request_line = format!("GET /api/fs/list?path={path} HTTP/1.1");
        let response =
            collect_handler_response(|stream| {
            handle_fs_list(stream, &request_line, fs_route_cors("/api/fs/list"), None)
        })
        .await;
        let canonical = std::fs::canonicalize(&file).unwrap();
        let body = serde_json::json!({
            "error": format!("{} is not a directory", canonical.display())
        })
        .to_string();
        assert_eq!(
            transcript(&response),
            golden_json_transcript("400 Bad Request", &body)
        );
    }

    /// Fixed fs-read fixture: 26 bytes, so the ranged expectations below
    /// can be written as literals.
    const GOLDEN_READ_CONTENT: &[u8] = b"golden transcript payload!";

    fn golden_read_fixture(dir: &tempfile::TempDir) -> (String, String) {
        let file = dir.path().join("golden.txt");
        std::fs::write(&file, GOLDEN_READ_CONTENT).unwrap();
        let path = file.to_string_lossy().into_owned();
        let request_line = format!("GET /api/fs/read?path={path} HTTP/1.1");
        (path, request_line)
    }

    #[tokio::test]
    async fn golden_fs_read_full_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let (_path, request_line) = golden_read_fixture(&dir);
        let header_text = format!("{request_line}\r\n\r\n");
        let response = collect_handler_response(|stream| {
            handle_fs_read(
                stream,
                &header_text,
                &request_line,
                fs_route_cors("/api/fs/read"),
                None,
            )
        })
        .await;
        let mut expected = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nX-Content-Sha256: {}\r\nContent-Disposition: attachment; filename=\"golden.txt\"\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Expose-Headers: X-Content-Sha256\r\nConnection: close\r\n\r\n",
            GOLDEN_READ_CONTENT.len(),
            fs_sha256_hex(GOLDEN_READ_CONTENT),
        )
        .into_bytes();
        expected.extend_from_slice(GOLDEN_READ_CONTENT);
        assert_eq!(transcript(&response), transcript(&expected));
    }

    #[tokio::test]
    async fn golden_fs_read_partial_206_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let (_path, request_line) = golden_read_fixture(&dir);
        let header_text = format!("{request_line}\r\nRange: bytes=7-16\r\n\r\n");
        let response = collect_handler_response(|stream| {
            handle_fs_read(
                stream,
                &header_text,
                &request_line,
                fs_route_cors("/api/fs/read"),
                None,
            )
        })
        .await;
        // A partial read carries Content-Range instead of the sha header.
        let mut expected = "HTTP/1.1 206 Partial Content\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 10\r\nAccept-Ranges: bytes\r\nContent-Range: bytes 7-16/26\r\nContent-Disposition: attachment; filename=\"golden.txt\"\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Expose-Headers: X-Content-Sha256\r\nConnection: close\r\n\r\n"
            .to_string()
            .into_bytes();
        expected.extend_from_slice(&GOLDEN_READ_CONTENT[7..17]);
        assert_eq!(transcript(&response), transcript(&expected));
    }

    #[tokio::test]
    async fn golden_fs_read_unsatisfiable_416_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let (_path, request_line) = golden_read_fixture(&dir);
        let header_text = format!("{request_line}\r\nRange: bytes=99-\r\n\r\n");
        let response = collect_handler_response(|stream| {
            handle_fs_read(
                stream,
                &header_text,
                &request_line,
                fs_route_cors("/api/fs/read"),
                None,
            )
        })
        .await;
        let body = r#"{"error":"range is not satisfiable"}"#;
        let expected = format!(
            "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nContent-Range: bytes */26\r\nAccept-Ranges: bytes\r\nCache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        assert_eq!(transcript(&response), expected);
    }

    #[tokio::test]
    async fn golden_fs_read_missing_404_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.bin");
        let path = missing.to_string_lossy().into_owned();
        let request_line = format!("GET /api/fs/read?path={path} HTTP/1.1");
        let header_text = format!("{request_line}\r\n\r\n");
        let response = collect_handler_response(|stream| {
            handle_fs_read(
                stream,
                &header_text,
                &request_line,
                fs_route_cors("/api/fs/read"),
                None,
            )
        })
        .await;
        let io_error = std::fs::canonicalize(&missing).unwrap_err();
        let body = serde_json::json!({
            "error": format!("{} is not accessible: {}", missing.display(), io_error)
        })
        .to_string();
        assert_eq!(
            transcript(&response),
            golden_json_transcript("404 Not Found", &body)
        );
    }

    // ── Golden HTTP transcripts: the fs mkdir/write/rename/delete wire
    //    contract (S1b) — same discipline as the GET trio above. ──

    /// The trusted-local root context dispatch hands the write quartet
    /// on a plain direct-dashboard request.
    fn golden_root_ctx() -> HttpAccessContext {
        HttpAccessContext {
            principal: crate::access::iam::AccessPrincipal::root_dashboard_session(
                "golden-test",
                "http",
            ),
            iam_state: None,
        }
    }

    #[tokio::test]
    async fn golden_fs_mkdir_created_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("new-dir");
        let body_text =
            serde_json::json!({ "path": target.to_string_lossy() }).to_string();
        let response = collect_handler_response(|stream| {
            handle_fs_mkdir(
                stream,
                body_text,
                golden_root_ctx(),
                None,
                EventBus::new(),
                fs_route_cors("/api/fs/mkdir"),
                None,
            )
        })
        .await;
        // Display path is canonicalized after creation.
        let display = std::fs::canonicalize(&target).unwrap();
        let body = serde_json::json!({
            "ok": true,
            "created": true,
            "already_exists": false,
            "path": display.to_string_lossy().to_string()
        })
        .to_string();
        assert_eq!(transcript(&response), golden_json_transcript("200 OK", &body));
    }

    #[tokio::test]
    async fn golden_fs_mkdir_conflict_409_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("occupied");
        std::fs::write(&file, b"file, not dir").unwrap();
        let body_text = serde_json::json!({ "path": file.to_string_lossy() }).to_string();
        let response = collect_handler_response(|stream| {
            handle_fs_mkdir(
                stream,
                body_text,
                golden_root_ctx(),
                None,
                EventBus::new(),
                fs_route_cors("/api/fs/mkdir"),
                None,
            )
        })
        .await;
        let body = serde_json::json!({
            "error": format!("{} already exists and is not a directory", file.display())
        })
        .to_string();
        assert_eq!(
            transcript(&response),
            golden_json_transcript("409 Conflict", &body)
        );
    }

    #[tokio::test]
    async fn golden_fs_mkdir_invalid_json_400_transcript() {
        let response = collect_handler_response(|stream| {
            handle_fs_mkdir(
                stream,
                "{}".to_string(),
                golden_root_ctx(),
                None,
                EventBus::new(),
                fs_route_cors("/api/fs/mkdir"),
                None,
            )
        })
        .await;
        let body = r#"{"error":"invalid JSON: missing field `path` at line 1 column 2"}"#;
        assert_eq!(
            transcript(&response),
            golden_json_transcript("400 Bad Request", body)
        );
    }

    #[tokio::test]
    async fn golden_fs_mkdir_scope_denied_403_transcript() {
        // A session-reader-scoped browser principal holds no
        // filesystem.write — the shared write-quartet 403 shape.
        let cert_dir = tempfile::tempdir().unwrap();
        let actor = crate::access::iam::AccessPrincipal::root_dashboard_session(
            "golden-test",
            "dashboard-control",
        );
        access_iam_upsert_user_client_grant_response_value_with_cert_dir(
            cert_dir.path(),
            serde_json::json!({
                "kind": "browser_certificate",
                "label": "Reader browser",
                "fingerprint": "F1:1E",
                "role_id": "role:session-reader"
            }),
            &actor,
        )
        .unwrap();
        let scoped = http_access_context(cert_dir.path(), None, Some("f11e"), true, true)
            .expect("scoped context");
        let reason = scoped
            .decision(crate::peer::access_policy::PeerOperation::FilesystemWrite)
            .reason;
        let dir = tempfile::tempdir().unwrap();
        let body_text = serde_json::json!({
            "path": dir.path().join("denied").to_string_lossy()
        })
        .to_string();
        let response = collect_handler_response(|stream| {
            handle_fs_mkdir(
                stream,
                body_text,
                scoped,
                None,
                EventBus::new(),
                fs_route_cors("/api/fs/mkdir"),
                None,
            )
        })
        .await;
        let body = serde_json::json!({ "error": reason }).to_string();
        assert_eq!(
            transcript(&response),
            golden_json_transcript("403 Forbidden", &body)
        );
    }

    #[tokio::test]
    async fn golden_fs_write_force_create_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("note.txt");
        let content = "s1b golden write body\n";
        let body_text = serde_json::json!({
            "path": target.to_string_lossy(),
            "content": content,
            "force": true
        })
        .to_string();
        let response = collect_handler_response(|stream| {
            handle_fs_write(
                stream,
                body_text,
                golden_root_ctx(),
                None,
                EventBus::new(),
                fs_route_cors("/api/fs/write"),
                None,
            )
        })
        .await;
        // New file: the handler resolved canonical parent + name.
        let resolved = std::fs::canonicalize(dir.path()).unwrap().join("note.txt");
        let modified_ms = std::fs::metadata(&resolved)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(system_time_unix_ms);
        let body = serde_json::json!({
            "ok": true,
            "path": resolved.to_string_lossy().to_string(),
            "size": content.len(),
            "sha256": fs_sha256_hex(content.as_bytes()),
            "created": true,
            "modified_ms": modified_ms,
        })
        .to_string();
        assert_eq!(transcript(&response), golden_json_transcript("200 OK", &body));
        assert_eq!(std::fs::read(&resolved).unwrap(), content.as_bytes());
    }

    #[tokio::test]
    async fn golden_fs_write_sha_conflict_409_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("edited.txt");
        std::fs::write(&target, b"current contents").unwrap();
        let body_text = serde_json::json!({
            "path": target.to_string_lossy(),
            "content": "replacement",
            "expected_sha256": fs_sha256_hex(b"what the client last read"),
        })
        .to_string();
        let response = collect_handler_response(|stream| {
            handle_fs_write(
                stream,
                body_text,
                golden_root_ctx(),
                None,
                EventBus::new(),
                fs_route_cors("/api/fs/write"),
                None,
            )
        })
        .await;
        let canonical = std::fs::canonicalize(&target).unwrap();
        let modified_ms = std::fs::metadata(&canonical)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(system_time_unix_ms);
        let body = serde_json::json!({
            "error": format!("{} changed on disk since it was read", canonical.display()),
            "code": "conflict",
            "current_sha256": fs_sha256_hex(b"current contents"),
            "size": b"current contents".len(),
            "modified_ms": modified_ms,
        })
        .to_string();
        assert_eq!(
            transcript(&response),
            golden_json_transcript("409 Conflict", &body)
        );
        // The failed write must not have touched the file.
        assert_eq!(std::fs::read(&canonical).unwrap(), b"current contents");
    }

    #[tokio::test]
    async fn golden_fs_write_precondition_required_400_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("bare.txt");
        let body_text = serde_json::json!({
            "path": target.to_string_lossy(),
            "content": "no precondition stated"
        })
        .to_string();
        let response = collect_handler_response(|stream| {
            handle_fs_write(
                stream,
                body_text,
                golden_root_ctx(),
                None,
                EventBus::new(),
                fs_route_cors("/api/fs/write"),
                None,
            )
        })
        .await;
        let body = serde_json::json!({
            "error": "write requires expected_sha256, create_new, or force",
            "code": "precondition_required",
        })
        .to_string();
        assert_eq!(
            transcript(&response),
            golden_json_transcript("400 Bad Request", &body)
        );
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn golden_fs_write_both_content_fields_400_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let body_text = serde_json::json!({
            "path": dir.path().join("dup.txt").to_string_lossy(),
            "content": "text",
            "content_b64": "dGV4dA==",
            "force": true
        })
        .to_string();
        let response = collect_handler_response(|stream| {
            handle_fs_write(
                stream,
                body_text,
                golden_root_ctx(),
                None,
                EventBus::new(),
                fs_route_cors("/api/fs/write"),
                None,
            )
        })
        .await;
        let body = r#"{"error":"provide either content or content_b64, not both"}"#;
        assert_eq!(
            transcript(&response),
            golden_json_transcript("400 Bad Request", body)
        );
    }

    #[tokio::test]
    async fn golden_fs_rename_success_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("old-name.txt");
        std::fs::write(&from, b"movable").unwrap();
        // Canonical source path must be captured while it still exists.
        let canonical_from = std::fs::canonicalize(&from).unwrap();
        let to = dir.path().join("new-name.txt");
        let body_text = serde_json::json!({
            "from": from.to_string_lossy(),
            "to": to.to_string_lossy()
        })
        .to_string();
        let response = collect_handler_response(|stream| {
            handle_fs_rename(
                stream,
                body_text,
                golden_root_ctx(),
                None,
                EventBus::new(),
                fs_route_cors("/api/fs/rename"),
                None,
            )
        })
        .await;
        let resolved_to = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("new-name.txt");
        let body = serde_json::json!({
            "ok": true,
            "from": canonical_from.to_string_lossy().to_string(),
            "path": resolved_to.to_string_lossy().to_string(),
            "renamed": true,
        })
        .to_string();
        assert_eq!(transcript(&response), golden_json_transcript("200 OK", &body));
        assert!(resolved_to.exists() && !from.exists());
    }

    #[tokio::test]
    async fn golden_fs_rename_destination_exists_409_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("src.txt");
        let to = dir.path().join("dst.txt");
        std::fs::write(&from, b"src").unwrap();
        std::fs::write(&to, b"dst").unwrap();
        let body_text = serde_json::json!({
            "from": from.to_string_lossy(),
            "to": to.to_string_lossy()
        })
        .to_string();
        let response = collect_handler_response(|stream| {
            handle_fs_rename(
                stream,
                body_text,
                golden_root_ctx(),
                None,
                EventBus::new(),
                fs_route_cors("/api/fs/rename"),
                None,
            )
        })
        .await;
        let resolved_to = std::fs::canonicalize(dir.path()).unwrap().join("dst.txt");
        let body = serde_json::json!({
            "error": format!("{} already exists", resolved_to.display()),
            "code": "exists",
        })
        .to_string();
        assert_eq!(
            transcript(&response),
            golden_json_transcript("409 Conflict", &body)
        );
    }

    #[tokio::test]
    async fn golden_fs_delete_file_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("removable.txt");
        std::fs::write(&target, b"bye").unwrap();
        let path = target.to_string_lossy().into_owned();
        let body_text = serde_json::json!({ "path": path }).to_string();
        let response = collect_handler_response(|stream| {
            handle_fs_delete(
                stream,
                body_text,
                golden_root_ctx(),
                None,
                EventBus::new(),
                fs_route_cors("/api/fs/delete"),
                None,
            )
        })
        .await;
        // Delete reports the expanded (not canonicalized) request path.
        let body = serde_json::json!({
            "ok": true,
            "path": path,
            "deleted": true,
            "dir": false,
        })
        .to_string();
        assert_eq!(transcript(&response), golden_json_transcript("200 OK", &body));
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn golden_fs_delete_not_empty_409_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("full-dir");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("kept.txt"), b"keep").unwrap();
        let path = target.to_string_lossy().into_owned();
        let body_text = serde_json::json!({ "path": path }).to_string();
        let response = collect_handler_response(|stream| {
            handle_fs_delete(
                stream,
                body_text,
                golden_root_ctx(),
                None,
                EventBus::new(),
                fs_route_cors("/api/fs/delete"),
                None,
            )
        })
        .await;
        let body = serde_json::json!({
            "error": format!(
                "{} is not empty — pass recursive to delete its contents",
                target.display()
            ),
            "code": "not_empty",
        })
        .to_string();
        assert_eq!(
            transcript(&response),
            golden_json_transcript("409 Conflict", &body)
        );
        assert!(target.join("kept.txt").exists());
    }
    // ── S4c golden transcripts: the staged-upload family (design §6 S4,
    // risk R1). The POST body rides the `discard` prefix (dispatch's
    // already-read bytes), so the duplex harness needs no writer side.

    async fn collect_upload_handler_response<Fut>(run: impl FnOnce(DemuxStream) -> Fut) -> Vec<u8>
    where
        Fut: std::future::Future<Output = ()>,
    {
        use tokio::io::AsyncReadExt;
        let (mut client, server) = tokio::io::duplex(1 << 20);
        run(Box::pin(server)).await;
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        response
    }

    fn upload_golden_tail() -> &'static str {
        "Cache-Control: no-cache\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n"
    }

    /// POST success over a project-rooted store: framing pinned exactly
    /// around the store-generated descriptor body.
    #[tokio::test]
    async fn golden_current_uploads_post_project_rooted_transcript() {
        let project = tempfile::tempdir().unwrap();
        let body = b"golden staged upload bytes".to_vec();
        let header_text = format!(
            "POST /api/session/current/uploads?name=golden.txt HTTP/1.1\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let bus = crate::event::EventBus::new();
        let root = project.path().to_path_buf();
        let response = collect_upload_handler_response(|stream| {
            handle_current_uploads_post(
                stream,
                &header_text,
                "POST /api/session/current/uploads?name=golden.txt HTTP/1.1",
                [header_text.as_bytes(), body.as_slice()].concat(),
                bus,
                Some(root),
                None,
                Some("golden-session".to_string()),
            )
        })
        .await;
        let text = String::from_utf8_lossy(&response);
        let (head, resp_body) = text.split_once("\r\n\r\n").expect("split");
        assert!(head.starts_with("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: "), "{text}");
        assert!(text.contains(upload_golden_tail()), "{head}");
        let descriptor: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        assert_eq!(descriptor["name"], "golden.txt");
        assert_eq!(descriptor["size"], body.len());
        assert!(descriptor["path"]
            .as_str()
            .unwrap()
            .starts_with(&project.path().to_string_lossy().to_string()));
    }

    /// POST success on a projectless daemon: the commit resolves the
    /// daemon-global store (PR #129 semantics), same wire framing.
    #[tokio::test]
    async fn golden_current_uploads_post_projectless_transcript() {
        let body = b"golden projectless upload bytes".to_vec();
        let session_id = format!(
            "golden-projectless-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let header_text = format!(
            "POST /api/session/current/uploads?name=global.txt HTTP/1.1\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let bus = crate::event::EventBus::new();
        let response = collect_upload_handler_response(|stream| {
            handle_current_uploads_post(
                stream,
                &header_text,
                "POST /api/session/current/uploads?name=global.txt HTTP/1.1",
                [header_text.as_bytes(), body.as_slice()].concat(),
                bus,
                None,
                None,
                Some(session_id.clone()),
            )
        })
        .await;
        let text = String::from_utf8_lossy(&response);
        let (head, resp_body) = text.split_once("\r\n\r\n").expect("split");
        assert!(head.starts_with("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: "), "{text}");
        assert!(text.contains(upload_golden_tail()), "{head}");
        let descriptor: serde_json::Value = serde_json::from_str(resp_body).unwrap();
        let store_root = crate::global_store::global_store_root();
        let path = descriptor["path"].as_str().unwrap().to_string();
        assert!(
            path.starts_with(&store_root.to_string_lossy().to_string()),
            "projectless upload must land in the global store: {path}"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path}.json"));
    }

    #[tokio::test]
    async fn golden_current_uploads_get_and_delete_transcripts() {
        let project = tempfile::tempdir().unwrap();
        // Empty list.
        let root = project.path().to_path_buf();
        let response = collect_upload_handler_response(|stream| {
            handle_current_uploads_get(
                stream,
                "GET /api/session/current/uploads HTTP/1.1",
                Some(root),
                None,
            )
        })
        .await;
        let expected = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n{}[]",
            upload_golden_tail()
        );
        assert_eq!(String::from_utf8_lossy(&response), expected);

        // Raw fetch of a missing upload.
        let root = project.path().to_path_buf();
        let response = collect_upload_handler_response(|stream| {
            handle_current_uploads_get(
                stream,
                "GET /api/session/current/uploads/nope/raw HTTP/1.1",
                Some(root),
                None,
            )
        })
        .await;
        let body = r#"{"error":"upload not found"}"#;
        let expected = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}{}",
            body.len(),
            upload_golden_tail(),
            body
        );
        assert_eq!(String::from_utf8_lossy(&response), expected);

        // Delete of an id that is not there stays idempotent-ok, under
        // the canonical json tail (json_response framing).
        let root = project.path().to_path_buf();
        let bus = crate::event::EventBus::new();
        let response = collect_upload_handler_response(|stream| {
            handle_current_upload_delete(
                stream,
                "DELETE /api/session/current/uploads/nope HTTP/1.1",
                bus,
                Some(root),
                None,
            )
        })
        .await;
        let body = r#"{"ok":true}"#;
        let expected = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        assert_eq!(String::from_utf8_lossy(&response), expected);
    }

}
