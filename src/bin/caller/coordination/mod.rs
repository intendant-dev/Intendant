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
//!    *Liveness amendment (C1)*: for the liveness kinds a malformed
//!    ENTRY is a named rejection carried in the scan result and the
//!    scan continues — one bad same-UID file must not blind the radar;
//!    scan-bound overflow stays corruption-grade (hard error). The
//!    checkpoint kind keeps the original all-or-nothing posture: a
//!    corrupt space must not half-restore a workflow.
//! 6. **Bounds are write-side too**: body and file-count caps reject
//!    loudly (named errors) instead of degrading.
//! 7. **Attribution is honest**: a writer records its session id, and
//!    the record explicitly carries the §8 posture — same-UID writers
//!    are NOT cryptographically distinguished; the field is
//!    informational, never authorization.
//! 8. **Checkpoint GC is acknowledgement-driven, never TTL**: a
//!    generation is removed ONLY when a successor supersedes it (the
//!    successor's own write is durable first) or a terminal record
//!    closes the workflow. *Liveness amendment (C1)*: the liveness
//!    kinds — and only they — age out on time (`gc::sweep_all`):
//!    declarations a day past their last heartbeat, messages past
//!    their TTL, orphaned atomic-write temps after an hour. The sweep
//!    never opens, ages, or deletes a checkpoint document, and never
//!    deletes what it cannot parse (malformed entries are kept and
//!    reported).
//! 9. **Daemonless cleanup**: `complete` (the terminal record) removes
//!    the workflow's generations; an abandoned space is inert bytes a
//!    human can delete — nothing replays or executes from it.
//!
//! The founding v0 kind — the **workflow checkpoint**
//! (`checkpoint.rs`) — replaces the tombed memory system's single live
//! orchestration duty (the orchestrator prompt's `project_state`
//! checkpoints): after each sub-agent completes, the orchestrator
//! persists "what's done / in flight / decided / constrained" so a
//! successor (post-compaction or post-restart) resumes with full
//! awareness.
//!
//! Track C (C1) extends the space with the **liveness kinds** — data
//! that describes who is working right now, and therefore expires:
//! `declarations.rs` (`sessions/` — one declaration per live session:
//! identity, intent, believed-dirty paths, heartbeat by mtime) and
//! `messages.rs` (`messages/<writer>/` — bounded TTL'd notes between
//! sessions; the `daemon` writer name is reserved for the daemon's
//! lanes). `scan.rs` carries the shared rule-5 liveness machinery and
//! field grammars, `paths.rs` the space-dir resolution seam
//! (`INTENDANT_COORDINATION_DIR` override → derived key), and `gc.rs`
//! the rule-8 liveness sweep. The consumers — collision radar,
//! daemon-rendered prompt lanes, the CLI — are C2/C3.

use std::path::{Path, PathBuf};

mod checkpoint;
pub(crate) use checkpoint::*;
pub(crate) mod declarations;
pub(crate) mod gc;
pub(crate) mod messages;
pub(crate) mod paths;
pub(crate) mod scan;

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
