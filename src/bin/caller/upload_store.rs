//! On-disk store for user-uploaded files.
//!
//! Uploads come in two flavours:
//!
//! - **Task-scoped** (the dashboard default). Stored under
//!   `<project_root>/.intendant/uploads/<session-id>/` so sandboxed
//!   external agents can read attachments with normal workspace access,
//!   without polluting the project's tracked files.
//! - **Workspace-durable** is a legacy API spelling retained for old
//!   dashboards. It now uses the same ignored `.intendant/uploads` store
//!   instead of the old `<project_root>/workspace_files/` directory.
//!
//! Every store function takes a [`StoreScope`]: project-rooted daemons use
//! the project-local store above; projectless daemons fall back to the
//! daemon-global store (`~/.intendant/global-store/uploads/<session-id>/`,
//! identical layout and sidecar format). The fallback store is pruned on
//! daemon startup after [`crate::global_store::GLOBAL_STORE_RETENTION_DAYS`]
//! days of inactivity — see `global_store.rs` for the resolution rule and
//! retention policy.
//!
//! The browser-facing POST endpoint picks the destination based on a
//! query param; both variants produce an [`UploadDescriptor`] that the
//! dashboard can attach to a task via `attachments: ["upload:<id>", ...]`
//! on [`crate::event::ControlMsg::StartTask`] / `FollowUp`.

use crate::error::CallerError;
use crate::global_store::StoreScope;
use serde::{Deserialize, Serialize};
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

/// User-requested upload scope. Kept in descriptors for compatibility, but
/// dashboard uploads now share the project-local ignored store regardless of
/// this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadDestination {
    /// Dropped under the project-local ignored `.intendant/uploads` store.
    Task,
    /// Legacy spelling; also uses the project-local ignored upload store.
    Workspace,
}

impl UploadDestination {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "task" => Some(Self::Task),
            "workspace" => Some(Self::Workspace),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Workspace => "workspace",
        }
    }
}

/// Descriptor for a single uploaded file, as returned by the upload endpoint
/// and broadcast via `AppEvent::UploadReady` / `OutboundEvent::UploadReady`.
///
/// The dashboard holds a list of these and passes `id`s back in
/// `ControlMsg::StartTask.attachments` (prefixed `upload:<id>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadDescriptor {
    /// Stable identifier. Currently a UUIDv4 as a hyphenated string.
    pub id: String,
    /// Original filename from the browser, sanitized for disk use.
    pub name: String,
    /// Original basename from the browser, preserved for display/model context.
    /// Older sidecars do not have this field.
    #[serde(default)]
    pub original_name: Option<String>,
    /// MIME type the browser sent in `Content-Type`, or
    /// `application/octet-stream` if none.
    pub mime: String,
    /// Size in bytes of the stored file.
    pub size: u64,
    /// Absolute path on disk where the bytes live.
    pub path: PathBuf,
    /// Task- vs workspace-scope.
    pub destination: UploadDestination,
    /// Session that owns this upload (mostly for Task scope; Workspace
    /// uploads still record which session created them for audit).
    pub session_id: String,
    /// Unix epoch seconds when the upload was created.
    pub created_at: u64,
}

impl UploadDescriptor {
    /// True if the upload is an image MIME type (image/png, image/jpeg, ...).
    /// Used by the agent delivery path to decide whether to pass via
    /// `localImage` / ACP image block or fall back to "stage + point".
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
}

/// Sanitize a user-supplied filename so it's safe to write to disk.
/// Strips any path separators, keeps the extension, and replaces anything
/// outside `[A-Za-z0-9._-]` with an underscore.
pub fn sanitize_name(raw: &str) -> String {
    // Strip any path component the browser may have sent (defence in depth;
    // File objects only expose the basename but we don't trust that).
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Collapse runs of "_" and strip leading dots so we can't write dotfiles.
    let cleaned = cleaned.trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        "upload.bin".to_string()
    } else {
        cleaned
    }
}

fn display_name(raw: &str, safe_name: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw).trim();
    if base.is_empty() || base == safe_name {
        None
    } else {
        Some(base.to_string())
    }
}

/// Directory where task-scoped uploads live.
fn legacy_task_uploads_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("uploads")
}

/// Legacy directory where workspace-durable uploads used to live.
fn legacy_workspace_uploads_dir(project_root: &Path) -> PathBuf {
    project_root.join("workspace_files")
}

/// Root of the upload store for a scope: the project-local ignored store
/// (`<project>/.intendant/uploads`) or the daemon-global fallback
/// (`<global-store>/uploads`). Same layout either way.
fn uploads_root(scope: &StoreScope) -> PathBuf {
    scope.store_base().join("uploads")
}

/// Directory for uploads associated with one Intendant wrapper/worker session.
fn session_uploads_dir(scope: &StoreScope, session_id: &str) -> PathBuf {
    let safe_session = sanitize_name(session_id);
    uploads_root(scope).join(safe_session)
}

fn ignore_rule_matches_intendant_uploads(line: &str) -> bool {
    let trimmed = line.split('#').next().unwrap_or("").trim();
    matches!(
        trimmed,
        ".intendant"
            | ".intendant/"
            | "/.intendant"
            | "/.intendant/"
            | ".intendant/uploads"
            | ".intendant/uploads/"
            | "/.intendant/uploads"
            | "/.intendant/uploads/"
            | ".intendant/**"
            | "/.intendant/**"
    )
}

fn ignore_file_has_intendant_rule(path: &Path) -> bool {
    fs::read_to_string(path)
        .map(|content| content.lines().any(ignore_rule_matches_intendant_uploads))
        .unwrap_or(false)
}

fn git_info_exclude_path(project_root: &Path) -> Option<PathBuf> {
    let mut dir = Some(project_root);
    while let Some(current) = dir {
        let dot_git = current.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git.join("info").join("exclude"));
        }
        if dot_git.is_file() {
            let content = fs::read_to_string(&dot_git).ok()?;
            let raw = content.strip_prefix("gitdir:")?.trim();
            let git_dir = {
                let path = PathBuf::from(raw);
                if path.is_absolute() {
                    path
                } else {
                    current.join(path)
                }
            };
            return Some(git_dir.join("info").join("exclude"));
        }
        dir = current.parent();
    }
    None
}

fn append_intendant_ignore_rule(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let needs_leading_newline = fs::read(path)
        .map(|bytes| !bytes.is_empty() && !bytes.ends_with(b"\n"))
        .unwrap_or(false);
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    if needs_leading_newline {
        file.write_all(b"\n")?;
    }
    file.write_all(b"# Intendant local uploads\n.intendant/\n")?;
    Ok(())
}

/// Ensure project-local uploads do not show up as untracked files.
///
/// Prefer Git's local exclude file so Intendant does not modify tracked
/// project metadata unless there is no Git metadata to attach to.
pub(crate) fn ensure_project_uploads_ignored(project_root: &Path) -> io::Result<()> {
    let project_gitignore = project_root.join(".gitignore");
    if ignore_file_has_intendant_rule(&project_gitignore) {
        return Ok(());
    }
    if let Some(exclude) = git_info_exclude_path(project_root) {
        if ignore_file_has_intendant_rule(&exclude) {
            return Ok(());
        }
        append_intendant_ignore_rule(&exclude)?;
        return Ok(());
    }
    append_intendant_ignore_rule(&project_gitignore)
}

/// Commit a pending temp file into the upload store as a new descriptor.
///
/// The caller is responsible for having streamed the bytes into a tempfile
/// with a size cap already applied (so we don't need to reread + measure
/// here). The tempfile is moved (rename-if-possible, otherwise copy+delete)
/// into the target directory under a unique name.
pub fn commit_upload(
    temp_file: tempfile::NamedTempFile,
    original_name: &str,
    mime: &str,
    size: u64,
    destination: UploadDestination,
    _session_dir: &Path,
    session_id: &str,
    scope: &StoreScope,
) -> Result<UploadDescriptor, CallerError> {
    let id = uuid::Uuid::new_v4().to_string();
    let safe_name = sanitize_name(original_name);
    let original_display_name = display_name(original_name, &safe_name);
    // Only project stores live inside a checkout; the global store needs
    // no ignore rule (it is daemon state, not project content).
    if let Some(project_root) = scope.project_root() {
        ensure_project_uploads_ignored(project_root)?;
    }
    let dir = session_uploads_dir(scope, session_id);
    fs::create_dir_all(&dir)?;

    // Filename layout:
    //   <id-prefix>__<safe_name>
    // The prefix stops clashes when two files share the same name (common:
    // "screenshot.png"), and keeps the extension intact so downstream tools
    // (agent file-read, OS preview) infer the type correctly.
    let prefix = &id[..id.len().min(8)];
    let filename = format!("{prefix}__{safe_name}");
    let dest_path = dir.join(&filename);

    // Prefer rename (atomic on the same filesystem); fall back to copy if
    // the tempdir lives elsewhere (common on Linux when TMPDIR is tmpfs
    // and the session dir is on a regular disk).
    crate::file_watcher::persist_tempfile(temp_file, &dest_path)?;

    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let descriptor = UploadDescriptor {
        id,
        name: safe_name,
        original_name: original_display_name,
        mime: mime.to_string(),
        size,
        path: dest_path,
        destination,
        session_id: session_id.to_string(),
        created_at,
    };

    // Write a sidecar .json next to each upload so we can rehydrate
    // descriptors after daemon restart without a central index.
    let sidecar = descriptor
        .path
        .with_extension(descriptor_extension(&descriptor.path));
    let json = serde_json::to_vec_pretty(&descriptor)?;
    if let Err(err) = crate::file_watcher::atomic_write(&sidecar, &json) {
        if let Err(cleanup_err) = fs::remove_file(&descriptor.path) {
            eprintln!(
                "[upload-store] failed to remove upload blob {} after sidecar write failed: {}",
                descriptor.path.display(),
                cleanup_err
            );
        }
        return Err(CallerError::Io(err));
    }

    Ok(descriptor)
}

/// Compute the sidecar `.json` path for a given upload path. Keeps both
/// files under the same basename (`<id>__<name>` and `<id>__<name>.json`)
/// so a `ls` of the upload dir lines them up.
fn descriptor_extension(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if !ext.is_empty() => format!("{ext}.json"),
        _ => "json".to_string(),
    }
}

/// Read all descriptors currently stored for a scope/session, including
/// legacy pre-.intendant upload locations. Order: newest first (by
/// `created_at`).
pub fn list_uploads(session_dir: &Path, scope: &StoreScope) -> Vec<UploadDescriptor> {
    let mut out: Vec<UploadDescriptor> = Vec::new();
    let mut dirs = vec![legacy_task_uploads_dir(session_dir)];
    if let Some(project_root) = scope.project_root() {
        // The legacy workspace store only ever existed inside projects.
        dirs.push(legacy_workspace_uploads_dir(project_root));
    }
    let root = uploads_root(scope);
    if let Ok(entries) = fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            }
        }
    }
    for dir in dirs {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // Sidecar JSON files end in `.ext.json` for typed uploads, or
            // just `.json` for extensionless uploads. Both match here.
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(descriptor) = serde_json::from_str::<UploadDescriptor>(&content) {
                    out.push(descriptor);
                }
            }
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out
}

/// Look up a single upload by id. `None` if no descriptor matches.
pub fn find_upload(id: &str, session_dir: &Path, scope: &StoreScope) -> Option<UploadDescriptor> {
    list_uploads(session_dir, scope)
        .into_iter()
        .find(|u| u.id == id)
        .or_else(|| find_task_upload_in_sibling_sessions(id, session_dir))
}

fn find_task_upload_in_sibling_sessions(id: &str, session_dir: &Path) -> Option<UploadDescriptor> {
    let sessions_root = session_dir.parent()?;
    let entries = fs::read_dir(sessions_root).ok()?;
    for entry in entries.flatten() {
        let sibling_dir = entry.path();
        if sibling_dir == session_dir || !sibling_dir.is_dir() {
            continue;
        }
        let uploads_dir = legacy_task_uploads_dir(&sibling_dir);
        let uploads = match fs::read_dir(&uploads_dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for upload_entry in uploads.flatten() {
            let path = upload_entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(descriptor) = serde_json::from_str::<UploadDescriptor>(&content) else {
                continue;
            };
            if descriptor.id == id {
                return Some(descriptor);
            }
        }
    }
    None
}

/// Remove an upload and its sidecar. Returns `Ok(false)` if no descriptor
/// matched (idempotent — the caller can treat "already gone" the same as
/// "just deleted").
pub fn delete_upload(id: &str, session_dir: &Path, scope: &StoreScope) -> io::Result<bool> {
    let Some(descriptor) = find_upload(id, session_dir, scope) else {
        return Ok(false);
    };
    let sidecar = descriptor
        .path
        .with_extension(descriptor_extension(&descriptor.path));
    let _ = fs::remove_file(&sidecar);
    match fs::remove_file(&descriptor.path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn mk_tempfile(bytes: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        f
    }

    fn project_scope(project_root: &Path) -> StoreScope {
        StoreScope::Project(project_root.to_path_buf())
    }

    #[test]
    fn sanitize_strips_path_components_and_bad_chars() {
        assert_eq!(sanitize_name("/etc/passwd"), "passwd");
        assert_eq!(sanitize_name("..\\..\\foo.txt"), "foo.txt");
        assert_eq!(sanitize_name("hello world!.txt"), "hello_world_.txt");
        assert_eq!(sanitize_name(""), "upload.bin");
        assert_eq!(sanitize_name("...."), "upload.bin");
    }

    #[test]
    fn commit_preserves_original_display_name_when_sanitized() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("session");
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();

        let descriptor = commit_upload(
            mk_tempfile(b"hello"),
            "duplicate name & symbols [one].txt",
            "text/plain",
            5,
            UploadDestination::Task,
            &session_dir,
            "sess-1",
            &project_scope(&project_root),
        )
        .unwrap();

        assert_eq!(descriptor.name, "duplicate_name___symbols__one_.txt");
        assert_eq!(
            descriptor.original_name.as_deref(),
            Some("duplicate name & symbols [one].txt")
        );
    }

    #[test]
    fn commit_and_list_task_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("session");
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();

        let pending = mk_tempfile(b"hello world");
        let descriptor = commit_upload(
            pending,
            "notes.txt",
            "text/plain",
            11,
            UploadDestination::Task,
            &session_dir,
            "sess-1",
            &project_scope(&project_root),
        )
        .unwrap();

        assert!(descriptor.path.exists(), "upload file must exist on disk");
        assert!(
            descriptor.path.starts_with(
                project_root
                    .join(".intendant")
                    .join("uploads")
                    .join("sess-1")
            ),
            "task-scope upload must live under project .intendant/uploads, got {}",
            descriptor.path.display()
        );
        assert_eq!(std::fs::read(&descriptor.path).unwrap(), b"hello world");
        assert!(ignore_file_has_intendant_rule(
            &project_root.join(".gitignore")
        ));

        let listed = list_uploads(&session_dir, &project_scope(&project_root));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, descriptor.id);
        assert_eq!(listed[0].name, "notes.txt");
        assert_eq!(listed[0].destination, UploadDestination::Task);
    }

    /// A projectless daemon's scope stores the same blob + sidecar layout
    /// under `<global-store>/uploads/<session-id>/`, with no git ignore
    /// metadata written anywhere, and the full commit/list/find/delete
    /// cycle works unchanged.
    #[test]
    fn global_scope_stores_under_global_store_root() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        let global_base = crate::global_store::global_store_root_in(tmp.path());
        let scope = StoreScope::Global(global_base.clone());

        let descriptor = commit_upload(
            mk_tempfile(b"projectless bytes"),
            "notes.txt",
            "text/plain",
            17,
            UploadDestination::Task,
            &session_dir,
            "sess-global",
            &scope,
        )
        .unwrap();

        assert!(
            descriptor
                .path
                .starts_with(global_base.join("uploads").join("sess-global")),
            "global-scope upload must live under <global-store>/uploads/<session>, got {}",
            descriptor.path.display()
        );
        assert_eq!(
            std::fs::read(&descriptor.path).unwrap(),
            b"projectless bytes"
        );
        // No project: no ignore metadata may be created.
        assert!(!tmp.path().join(".gitignore").exists());
        assert!(!global_base.join(".gitignore").exists());

        let listed = list_uploads(&session_dir, &scope);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, descriptor.id);

        let found = find_upload(&descriptor.id, &session_dir, &scope).unwrap();
        assert_eq!(found.path, descriptor.path);

        assert!(delete_upload(&descriptor.id, &session_dir, &scope).unwrap());
        assert!(!descriptor.path.exists());
    }

    #[test]
    fn find_upload_falls_back_to_sibling_task_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let first_session_dir = tmp.path().join("sessions").join("session-1");
        let second_session_dir = tmp.path().join("sessions").join("session-2");
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&first_session_dir).unwrap();
        std::fs::create_dir_all(&second_session_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();

        let pending = mk_tempfile(b"from old active session");
        let descriptor = commit_upload(
            pending,
            "handoff.txt",
            "text/plain",
            23,
            UploadDestination::Task,
            &first_session_dir,
            "session-1",
            &project_scope(&project_root),
        )
        .unwrap();

        let found = find_upload(
            &descriptor.id,
            &second_session_dir,
            &project_scope(&project_root),
        )
        .unwrap();
        assert_eq!(found.id, descriptor.id);
        assert_eq!(found.path, descriptor.path);
        assert_eq!(found.destination, UploadDestination::Task);
    }

    #[test]
    fn legacy_workspace_scope_uses_project_upload_store() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("session");
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();

        let pending = mk_tempfile(b"pdf bytes");
        let descriptor = commit_upload(
            pending,
            "report.pdf",
            "application/pdf",
            9,
            UploadDestination::Workspace,
            &session_dir,
            "sess-1",
            &project_scope(&project_root),
        )
        .unwrap();

        assert!(
            descriptor.path.starts_with(
                project_root
                    .join(".intendant")
                    .join("uploads")
                    .join("sess-1")
            ),
            "legacy workspace upload must land under project .intendant/uploads, got {}",
            descriptor.path.display()
        );
        // Agent path: file is directly readable via the agent's file-read
        // tool because it's inside the project root.
        assert_eq!(std::fs::read(&descriptor.path).unwrap(), b"pdf bytes");
    }

    #[test]
    fn ignore_rule_prefers_git_info_exclude() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("session");
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(project_root.join(".git").join("info")).unwrap();

        let pending = mk_tempfile(b"ignore me");
        let descriptor = commit_upload(
            pending,
            "ignore.txt",
            "text/plain",
            9,
            UploadDestination::Task,
            &session_dir,
            "sess-1",
            &project_scope(&project_root),
        )
        .unwrap();

        assert!(descriptor.path.exists());
        assert!(!project_root.join(".gitignore").exists());
        assert!(ignore_file_has_intendant_rule(
            &project_root.join(".git").join("info").join("exclude")
        ));
    }

    #[test]
    fn delete_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("session");
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();

        let pending = mk_tempfile(b"bye");
        let descriptor = commit_upload(
            pending,
            "gone.txt",
            "text/plain",
            3,
            UploadDestination::Task,
            &session_dir,
            "sess-1",
            &project_scope(&project_root),
        )
        .unwrap();

        assert!(
            delete_upload(&descriptor.id, &session_dir, &project_scope(&project_root)).unwrap()
        );
        assert!(!descriptor.path.exists());
        // Second delete: also Ok, returns false.
        assert!(
            !delete_upload(&descriptor.id, &session_dir, &project_scope(&project_root)).unwrap()
        );
    }

    #[test]
    fn is_image_matches_mime_prefix() {
        let mut d = UploadDescriptor {
            id: "x".into(),
            name: "a".into(),
            original_name: None,
            mime: "image/png".into(),
            size: 0,
            path: PathBuf::new(),
            destination: UploadDestination::Task,
            session_id: "s".into(),
            created_at: 0,
        };
        assert!(d.is_image());
        d.mime = "application/pdf".into();
        assert!(!d.is_image());
        d.mime = "text/plain".into();
        assert!(!d.is_image());
    }
}
