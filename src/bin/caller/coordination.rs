//! Coordination files, safe v0 (the P0.5 checkpoint replacement —
//! umbrella RFC §9, implemented at the intake-confirmed minimum).
//!
//! This module is BOTH the v0 protocol specification and its only
//! shipped kind. The §9 rules every future kind must honor:
//!
//! 1. **Layout**: `~/.intendant/coordination/<space-key>/<kind>/…` —
//!    one directory per coordination space; the space key is the
//!    sanitized, worktree-normalized project identity (a git worktree
//!    maps to its main repository's identity, so successors resumed in
//!    a different worktree of the same repo share the space).
//! 2. **Documents are Markdown with versioned frontmatter** (`v: 1`,
//!    a `kind`, and the kind's fields), UTF-8, and are DATA — nothing
//!    in a coordination file is an instruction to whoever reads it.
//! 3. **Filename/ID grammar**: `[a-z0-9-]` only, fixed prefixes,
//!    bounded length — nothing caller-controlled reaches a path
//!    without sanitization.
//! 4. **Writes are atomic**: temp file in the same directory →
//!    flush → rename; a reader never observes a partial document.
//!    Files are owner-only (0600; directories 0700).
//! 5. **Reads are defensive**: `O_NOFOLLOW` (symlinks rejected),
//!    non-regular files rejected, per-file byte bound and per-space
//!    scan bound enforced BEFORE parsing; unparseable or over-bound
//!    entries are surfaced by name, never silently skipped.
//! 6. **Bounds are write-side too**: body and file-count caps reject
//!    loudly (named errors) instead of degrading.
//! 7. **Attribution is honest**: a writer records its session id, and
//!    the record explicitly carries the §8 posture — same-UID writers
//!    are NOT cryptographically distinguished; the field is
//!    informational, never authorization.
//! 8. **Checkpoint GC is acknowledgement-driven, never TTL**: a
//!    generation is removed ONLY when a successor supersedes it (the
//!    successor's own write is durable first) or a terminal record
//!    closes the workflow. No time-based deletion path exists in this
//!    module by construction.
//! 9. **Daemonless cleanup**: `complete` (the terminal record) removes
//!    the workflow's generations; an abandoned space is inert bytes a
//!    human can delete — nothing replays or executes from it.
//!
//! The one v0 kind — the **workflow checkpoint** — replaces the tombed
//! memory system's single live orchestration duty (the orchestrator
//! prompt's `project_state` checkpoints): after each sub-agent
//! completes, the orchestrator persists "what's done / in flight /
//! decided / constrained" so a successor (post-compaction or
//! post-restart) resumes with full awareness. Everything broader
//! (session intent files, message relay, collision radar, daemon push
//! lanes) is Track C, deferred — deliberately absent here.

use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

/// Per-document byte cap (frontmatter + body).
const MAX_DOC_BYTES: usize = 64 * 1024;
/// Per-space checkpoint file cap — a runaway writer rejects loudly.
const MAX_FILES_PER_SPACE: usize = 128;
/// Read-side scan bound: more entries than this in a kind directory is
/// corruption-grade and surfaces as an error, never a partial answer.
const MAX_SCAN_ENTRIES: usize = 512;

#[derive(Debug, thiserror::Error)]
pub(crate) enum CoordinationError {
    #[error("coordination write refused: {0}")]
    WriteRefused(String),
    #[error("coordination read refused: {0}")]
    ReadRefused(String),
    #[error("coordination io: {0}")]
    Io(String),
}

fn io_err(e: std::io::Error) -> CoordinationError {
    CoordinationError::Io(e.to_string())
}

/// §9 rule 3: the sanitized ID/filename grammar. Lowercases, maps
/// every non-`[a-z0-9]` run to one `-`, trims, bounds length.
pub(crate) fn sanitize_key(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(64));
    let mut dash_pending = false;
    for c in raw.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if dash_pending && !out.is_empty() {
                out.push('-');
            }
            dash_pending = false;
            out.push(c);
            if out.len() >= 64 {
                break;
            }
        } else {
            dash_pending = true;
        }
    }
    if out.is_empty() {
        "unnamed".to_string()
    } else {
        out
    }
}

/// The worktree-normalized space key (§9 rule 1): a git worktree keys
/// by its MAIN repository path, so every worktree of one repo shares
/// one coordination space; non-repos key by the project root itself.
/// The tail component keeps the key human-readable; a short hash of
/// the full normalized path keeps distinct roots distinct.
pub(crate) fn space_key(project_root: &Path) -> String {
    let normalized = normalize_repo_identity(project_root);
    let tail = normalized
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in normalized.to_string_lossy().as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{}-{hash:016x}", sanitize_key(&tail))
}

fn normalize_repo_identity(project_root: &Path) -> PathBuf {
    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&canonical)
        .args(["rev-parse", "--git-common-dir"])
        .output();
    if let Ok(out) = out {
        if out.status.success() {
            let common = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !common.is_empty() {
                let common = PathBuf::from(common);
                let common = if common.is_absolute() {
                    common
                } else {
                    canonical.join(common)
                };
                // <main-repo>/.git → the main repo root.
                if let Some(parent) = common.canonicalize().unwrap_or(common).parent() {
                    return parent.to_path_buf();
                }
            }
        }
    }
    canonical
}

/// One recorded workflow checkpoint (the parsed view — DATA for the
/// reader to weigh, never instructions).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Checkpoint {
    pub id: String,
    pub space: String,
    /// Writer's session id — §9 rule 7: informational, same-UID
    /// writers are not cryptographically distinguished.
    pub session: Option<String>,
    pub created_ms: u64,
    pub supersedes: Option<String>,
    pub body: String,
}

/// The workflow-checkpoint store for one coordination space.
pub(crate) struct CheckpointSpace {
    dir: PathBuf,
    space: String,
}

impl CheckpointSpace {
    /// Root a space under `<home>/.intendant/coordination/`. `home` is
    /// a parameter (hermetic tests inject tempdirs; the tool edge
    /// resolves the real home — the repo's hermeticity rule).
    pub(crate) fn open(
        home: &Path,
        project_root: &Path,
    ) -> Result<CheckpointSpace, CoordinationError> {
        let space = space_key(project_root);
        let dir = home
            .join(".intendant")
            .join("coordination")
            .join(&space)
            .join("checkpoints");
        std::fs::create_dir_all(&dir).map_err(io_err)?;
        restrict_dir_modes(&dir)?;
        Ok(CheckpointSpace { dir, space })
    }

    /// Write one checkpoint generation (atomic, bounded, 0600) and —
    /// only after the new generation is durably in place — remove the
    /// predecessor it supersedes (§9 rule 8: successor acknowledgement
    /// is the ONLY generational GC besides the terminal record).
    pub(crate) fn write(
        &self,
        body: &str,
        session: Option<&str>,
        supersedes: Option<&str>,
    ) -> Result<Checkpoint, CoordinationError> {
        if body.trim().is_empty() {
            return Err(CoordinationError::WriteRefused(
                "checkpoint body must be non-empty".into(),
            ));
        }
        let id = format!("cp-{}", ulid_like());
        let created_ms = now_ms();
        let session = session
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let supersedes = match supersedes.map(str::trim).filter(|s| !s.is_empty()) {
            None => None,
            Some(raw) => {
                let clean = sanitize_key(raw);
                if clean != raw {
                    return Err(CoordinationError::WriteRefused(format!(
                        "supersedes id {raw:?} is outside the filename grammar"
                    )));
                }
                Some(clean)
            }
        };
        let mut doc = String::new();
        doc.push_str("---\n");
        doc.push_str("v: 1\n");
        doc.push_str("kind: workflow-checkpoint\n");
        doc.push_str(&format!("id: {id}\n"));
        doc.push_str(&format!("space: {}\n", self.space));
        if let Some(s) = &session {
            doc.push_str(&format!("session: {}\n", sanitize_key(s)));
        }
        doc.push_str(&format!("created_ms: {created_ms}\n"));
        if let Some(s) = &supersedes {
            doc.push_str(&format!("supersedes: {s}\n"));
        }
        // §9 rule 7 — stated in every document, not just the module doc.
        doc.push_str("attribution: unverified-same-uid\n");
        doc.push_str("---\n");
        doc.push_str(body);
        doc.push('\n');
        if doc.len() > MAX_DOC_BYTES {
            return Err(CoordinationError::WriteRefused(format!(
                "checkpoint document is {} bytes; the §9 bound is {MAX_DOC_BYTES}",
                doc.len()
            )));
        }
        let existing = self.scan_ids()?;
        if existing.len() >= MAX_FILES_PER_SPACE {
            return Err(CoordinationError::WriteRefused(format!(
                "space holds {} checkpoints; the §9 bound is {MAX_FILES_PER_SPACE} — \
                 complete the workflow or supersede older generations",
                existing.len()
            )));
        }

        // Atomic: temp in the same dir → flush → rename.
        let path = self.dir.join(format!("{id}.md"));
        let tmp = self.dir.join(format!(".{id}.tmp"));
        {
            let mut f = std::fs::File::create(&tmp).map_err(io_err)?;
            f.write_all(doc.as_bytes()).map_err(io_err)?;
            f.sync_all().map_err(io_err)?;
        }
        restrict_file_modes(&tmp)?;
        std::fs::rename(&tmp, &path).map_err(io_err)?;

        // Successor acknowledgement: the predecessor goes ONLY now,
        // with the new generation durable.
        if let Some(old) = &supersedes {
            let old_path = self.dir.join(format!("{old}.md"));
            if old_path != path && old_path.exists() {
                std::fs::remove_file(&old_path).map_err(io_err)?;
            }
        }

        Ok(Checkpoint {
            id,
            space: self.space.clone(),
            session,
            created_ms,
            supersedes,
            body: body.to_string(),
        })
    }

    /// The latest generation (lexicographically greatest id — the id
    /// embeds a sortable timestamp), or `None` on a fresh space.
    pub(crate) fn latest(&self) -> Result<Option<Checkpoint>, CoordinationError> {
        let mut ids = self.scan_ids()?;
        ids.sort();
        let Some(id) = ids.pop() else {
            return Ok(None);
        };
        self.read(&id).map(Some)
    }

    /// The terminal record (§9 rule 9): the workflow is done — every
    /// generation is removed. Explicit, human-auditable, never timed.
    pub(crate) fn complete(&self) -> Result<usize, CoordinationError> {
        let ids = self.scan_ids()?;
        let n = ids.len();
        for id in ids {
            std::fs::remove_file(self.dir.join(format!("{id}.md"))).map_err(io_err)?;
        }
        Ok(n)
    }

    fn read(&self, id: &str) -> Result<Checkpoint, CoordinationError> {
        let path = self.dir.join(format!("{id}.md"));
        let bytes = open_defensive(&path)?;
        let text = String::from_utf8(bytes)
            .map_err(|_| CoordinationError::ReadRefused(format!("{id}: not UTF-8")))?;
        let rest = text
            .strip_prefix("---\n")
            .ok_or_else(|| CoordinationError::ReadRefused(format!("{id}: missing frontmatter")))?;
        let (front, body) = rest.split_once("\n---\n").ok_or_else(|| {
            CoordinationError::ReadRefused(format!("{id}: unterminated frontmatter"))
        })?;
        let field = |k: &str| -> Option<String> {
            front.lines().find_map(|l| {
                l.strip_prefix(&format!("{k}: "))
                    .map(|v| v.trim().to_string())
            })
        };
        if field("v").as_deref() != Some("1") {
            return Err(CoordinationError::ReadRefused(format!(
                "{id}: frontmatter version {:?} is newer than this build",
                field("v")
            )));
        }
        if field("kind").as_deref() != Some("workflow-checkpoint") {
            return Err(CoordinationError::ReadRefused(format!(
                "{id}: kind {:?} is not workflow-checkpoint",
                field("kind")
            )));
        }
        Ok(Checkpoint {
            id: id.to_string(),
            space: field("space").unwrap_or_else(|| self.space.clone()),
            session: field("session"),
            created_ms: field("created_ms")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            supersedes: field("supersedes"),
            body: body.trim_end_matches('\n').to_string(),
        })
    }

    /// Enumerate checkpoint ids: bounded scan, grammar-checked names,
    /// non-regular entries surfaced (never silently skipped).
    fn scan_ids(&self) -> Result<Vec<String>, CoordinationError> {
        let mut ids = Vec::new();
        let entries = std::fs::read_dir(&self.dir).map_err(io_err)?;
        for (n, entry) in entries.enumerate() {
            if n >= MAX_SCAN_ENTRIES {
                return Err(CoordinationError::ReadRefused(format!(
                    "space exceeds the {MAX_SCAN_ENTRIES}-entry scan bound"
                )));
            }
            let entry = entry.map_err(io_err)?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue; // our own tmp files
            }
            let Some(id) = name.strip_suffix(".md") else {
                return Err(CoordinationError::ReadRefused(format!(
                    "foreign entry {name:?} in the checkpoint space"
                )));
            };
            if sanitize_key(id) != id {
                return Err(CoordinationError::ReadRefused(format!(
                    "entry {name:?} is outside the filename grammar"
                )));
            }
            // Symlinks/non-regular surface by name (lstat — no follow).
            let meta = std::fs::symlink_metadata(entry.path()).map_err(io_err)?;
            if !meta.is_file() {
                return Err(CoordinationError::ReadRefused(format!(
                    "entry {name:?} is not a regular file"
                )));
            }
            ids.push(id.to_string());
        }
        Ok(ids)
    }
}

/// §9 rule 5: no-follow open + non-regular rejection + byte bound,
/// checked BEFORE any parsing.
fn open_defensive(path: &Path) -> Result<Vec<u8>, CoordinationError> {
    use std::io::Read;
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = opts
        .open(path)
        .map_err(|e| CoordinationError::ReadRefused(format!("{}: {e}", path.display())))?;
    let meta = f.metadata().map_err(io_err)?;
    if !meta.is_file() {
        return Err(CoordinationError::ReadRefused(format!(
            "{}: not a regular file",
            path.display()
        )));
    }
    if meta.len() as usize > MAX_DOC_BYTES {
        return Err(CoordinationError::ReadRefused(format!(
            "{}: {} bytes exceeds the §9 bound {MAX_DOC_BYTES}",
            path.display(),
            meta.len()
        )));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    f.read_to_end(&mut bytes).map_err(io_err)?;
    Ok(bytes)
}

#[cfg(unix)]
fn restrict_dir_modes(dir: &Path) -> Result<(), CoordinationError> {
    use std::os::unix::fs::PermissionsExt;
    for p in [dir, dir.parent().unwrap_or(dir)] {
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}
#[cfg(not(unix))]
fn restrict_dir_modes(_dir: &Path) -> Result<(), CoordinationError> {
    Ok(())
}

#[cfg(unix)]
fn restrict_file_modes(path: &Path) -> Result<(), CoordinationError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io_err)
}
#[cfg(not(unix))]
fn restrict_file_modes(_path: &Path) -> Result<(), CoordinationError> {
    Ok(())
}

/// Sortable, collision-resistant id: ms timestamp + random tail, in
/// the `[a-z0-9]` grammar (crockford-ish base32, lowercase).
fn ulid_like() -> String {
    use rand::RngCore;
    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let ms = now_ms();
    let mut out = String::with_capacity(24);
    for shift in (0..48).step_by(5).rev() {
        out.push(ALPHABET[((ms >> shift) & 0x1f) as usize] as char);
    }
    let mut r = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut r);
    for b in r {
        out.push(ALPHABET[(b & 0x1f) as usize] as char);
    }
    out
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(tmp: &tempfile::TempDir) -> CheckpointSpace {
        CheckpointSpace::open(tmp.path(), &tmp.path().join("project")).unwrap()
    }

    #[test]
    fn write_read_supersede_complete_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let cp = space(&tmp);
        assert!(cp.latest().unwrap().is_none(), "fresh space is empty");

        let first = cp.write("phase 1 done", Some("sess-1"), None).unwrap();
        let second = cp
            .write("phase 2 done", Some("sess-1"), Some(&first.id))
            .unwrap();
        // Successor acknowledgement removed the predecessor.
        let latest = cp.latest().unwrap().expect("a checkpoint");
        assert_eq!(latest.id, second.id);
        assert_eq!(latest.body, "phase 2 done");
        assert_eq!(latest.supersedes.as_deref(), Some(first.id.as_str()));
        assert_eq!(
            cp.scan_ids().unwrap().len(),
            1,
            "old generation GC'd on ack"
        );

        // Terminal record clears the space.
        assert_eq!(cp.complete().unwrap(), 1);
        assert!(cp.latest().unwrap().is_none());
    }

    /// §9 rule 8: WITHOUT acknowledgement, generations accumulate —
    /// nothing in this module deletes by age (no TTL path exists).
    #[test]
    fn unacknowledged_generations_are_never_reaped() {
        let tmp = tempfile::tempdir().unwrap();
        let cp = space(&tmp);
        cp.write("gen 1", None, None).unwrap();
        cp.write("gen 2", None, None).unwrap();
        cp.write("gen 3", None, None).unwrap();
        assert_eq!(cp.scan_ids().unwrap().len(), 3);
    }

    /// Atomicity: a reader never sees a partial document — the temp
    /// file is dot-prefixed (skipped by scans) until the rename.
    #[test]
    fn scans_skip_in_flight_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let cp = space(&tmp);
        std::fs::write(cp.dir.join(".cp-inflight.tmp"), "partial").unwrap();
        cp.write("real checkpoint", None, None).unwrap();
        assert_eq!(cp.scan_ids().unwrap().len(), 1);
        assert_eq!(cp.latest().unwrap().unwrap().body, "real checkpoint");
    }

    /// §9 rule 5: symlinked and foreign entries surface by name.
    #[cfg(unix)]
    #[test]
    fn symlinks_and_foreign_entries_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let cp = space(&tmp);
        cp.write("legit", None, None).unwrap();
        let target = tmp.path().join("outside.md");
        std::fs::write(&target, "---\nv: 1\n---\nowned elsewhere").unwrap();
        std::os::unix::fs::symlink(&target, cp.dir.join("cp-evil.md")).unwrap();
        let err = cp.latest().unwrap_err();
        assert!(
            err.to_string().contains("not a regular file"),
            "symlink must surface: {err}"
        );
        std::fs::remove_file(cp.dir.join("cp-evil.md")).unwrap();
        std::fs::write(cp.dir.join("notes.txt"), "stray").unwrap();
        let err = cp.latest().unwrap_err();
        assert!(err.to_string().contains("foreign entry"), "{err}");
    }

    /// §9 rules 3/6: grammar violations and bounds reject loudly.
    #[test]
    fn bounds_and_grammar_reject_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let cp = space(&tmp);
        let big = "x".repeat(MAX_DOC_BYTES + 1);
        let err = cp.write(&big, None, None).unwrap_err();
        assert!(err.to_string().contains("bound"), "{err}");
        let err = cp.write("ok", None, Some("../escape")).unwrap_err();
        assert!(err.to_string().contains("grammar"), "{err}");
        assert_eq!(sanitize_key("Hello, World! ../.."), "hello-world");
    }

    /// §9 rule 1: every worktree of one repository shares one space;
    /// distinct repositories get distinct spaces.
    #[test]
    fn space_key_normalizes_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str], cwd: &Path| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q", "-b", "main"], &repo);
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "seed",
            ],
            &repo,
        );
        let wt = tmp.path().join("wt");
        git(&["worktree", "add", "-q", wt.to_str().unwrap()], &repo);

        assert_eq!(
            space_key(&repo),
            space_key(&wt),
            "worktree shares the repo space"
        );
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        assert_ne!(space_key(&repo), space_key(&other));
    }

    /// The document format round-trips its fields and states the §9
    /// attribution posture in every file.
    #[test]
    fn document_carries_versioned_frontmatter_and_posture() {
        let tmp = tempfile::tempdir().unwrap();
        let cp = space(&tmp);
        let written = cp.write("the body", Some("sess-42"), None).unwrap();
        let raw = std::fs::read_to_string(cp.dir.join(format!("{}.md", written.id))).unwrap();
        assert!(raw.starts_with("---\nv: 1\nkind: workflow-checkpoint\n"));
        assert!(raw.contains("attribution: unverified-same-uid"));
        let back = cp.latest().unwrap().unwrap();
        assert_eq!(back.session.as_deref(), Some("sess-42"));
        assert_eq!(back.body, "the body");
    }

    /// 0600/0700 modes on files and directories (unix).
    #[cfg(unix)]
    #[test]
    fn modes_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let cp = space(&tmp);
        let written = cp.write("body", None, None).unwrap();
        let fmode = std::fs::metadata(cp.dir.join(format!("{}.md", written.id)))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(fmode & 0o777, 0o600);
        let dmode = std::fs::metadata(&cp.dir).unwrap().permissions().mode();
        assert_eq!(dmode & 0o777, 0o700);
    }
}
