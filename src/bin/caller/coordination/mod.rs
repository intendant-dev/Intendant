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
//! (`INTENDANT_COORDINATION_DIR` override → derived key), `gc.rs` the
//! rule-8 liveness sweep, and `lifecycle.rs` the supervised-session
//! declaration glue (declare at start / heartbeat at loop boundaries /
//! remove on clean end) the native and wrapper loops own. The
//! consumers — collision radar, daemon-rendered prompt lanes, the
//! CLI — are C2/C3.

use std::path::{Path, PathBuf};

mod checkpoint;
pub(crate) use checkpoint::*;
pub(crate) mod declarations;
pub(crate) mod gc;
pub(crate) mod lifecycle;
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

/// Process clock in epoch ms — `pub(crate)` so the loop edges can pass
/// the same clock the stores use into declare/heartbeat calls.
pub(crate) fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// R5 rider (ruled): the `intendant-coordination` skill ships a python3
/// zero-binary fallback — a DUPLICATED derivation and writer that can
/// drift silently, and a drifted guest writes to a wrong space and
/// splits the bus. This pin extracts the canonical snippet from the
/// shipped SKILL.md (failing loudly if the fence moves) and holds it
/// against the Rust implementation: same space key on fixture roots,
/// and python-written documents parsing byte-perfectly through the
/// Rust scanners. Skips LOUDLY when no python is on PATH (the CI fleet
/// carries one). Hermetic: tempdir spaces passed as argv, no env
/// mutation.
#[cfg(test)]
mod skill_parity_tests {
    use super::declarations::DeclarationSpace;
    use super::messages::{MessageSpace, MESSAGE_TTL_DEFAULT_S};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// The one canonical ```python fence in the skill.
    fn canonical_snippet() -> String {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/intendant-coordination/SKILL.md");
        let md =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let mut blocks = md.split("```python\n").skip(1);
        let block = blocks.next().unwrap_or_else(|| {
            panic!(
                "SKILL.md lost its ```python fence — the R5 parity pin needs the canonical snippet"
            )
        });
        assert!(
            blocks.next().is_none(),
            "more than one ```python fence in SKILL.md — keep ONE canonical snippet for the parity pin"
        );
        block
            .split("\n```")
            .next()
            .expect("fence terminated")
            .to_string()
    }

    fn python() -> Option<&'static str> {
        ["python3", "python"].into_iter().find(|candidate| {
            Command::new(candidate)
                .arg("--version")
                .output()
                .map(|out| out.status.success())
                .unwrap_or(false)
        })
    }

    fn run_snippet(py: &str, script: &Path, cwd: &Path, args: &[&str]) -> String {
        let out = Command::new(py)
            .arg(script)
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("python spawns");
        assert!(
            out.status.success(),
            "python {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("snippet output is UTF-8")
            .trim()
            .to_string()
    }

    fn git(cwd: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn skill_snippet_matches_rust_derivation_and_parsers() {
        let Some(py) = python() else {
            eprintln!(
                "SKIPPED: neither `python3` nor `python` on PATH — the R5 \
                 skill-snippet parity pin DID NOT RUN"
            );
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("bus.py");
        std::fs::write(&script, canonical_snippet()).unwrap();

        // 1. Space-key derivation parity (R5's condition). Non-repo
        //    roots exercise sanitize + FNV over the canonical path; the
        //    repo + linked worktree pair exercises the git-common-dir
        //    normalization on both sides.
        let mut fixtures: Vec<PathBuf> = Vec::new();
        for name in ["My Project X", "plain", "héllo wörld 42"] {
            let root = tmp.path().join(name);
            std::fs::create_dir_all(&root).unwrap();
            fixtures.push(root);
        }
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let repo_ready = git(&repo, &["init", "-q", "-b", "main"])
            && git(
                &repo,
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
            );
        let wt = tmp.path().join("wt");
        if repo_ready && git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]) {
            fixtures.push(repo);
            fixtures.push(wt);
        } else {
            eprintln!("SKIPPED git fixtures: no usable `git` on PATH — non-repo parity still ran");
        }
        for root in &fixtures {
            let line = run_snippet(py, &script, tmp.path(), &["dir", root.to_str().unwrap()]);
            let py_key = Path::new(&line)
                .file_name()
                .expect("printed dir has a basename")
                .to_string_lossy()
                .to_string();
            assert_eq!(
                py_key,
                super::space_key(root),
                "python derivation drifted from space_key for {}",
                root.display()
            );
        }

        // 2. Guest writer id: minted once, grammar-idempotent, off the
        //    supervised `s-` and reserved `daemon` namespaces.
        let writer = run_snippet(py, &script, tmp.path(), &["mint"]);
        assert!(writer.starts_with("guest-"), "{writer}");
        assert_eq!(super::sanitize_key(&writer), writer, "id obeys the grammar");

        // 3. Written-document parity: the python declaration and message
        //    parse byte-perfectly through the Rust scanners — ids,
        //    fields, intent, body, zero rejects.
        let space = tmp.path().join("parity-space");
        let space_arg = space.to_str().unwrap();
        let intent = "refactor the encoder pool; hands off crates/intendant-display";
        run_snippet(
            py,
            &script,
            tmp.path(),
            &[
                "declare",
                space_arg,
                &writer,
                intent,
                "src/a.rs",
                "docs/plan.md",
            ],
        );
        let now = super::now_ms();
        let ds = DeclarationSpace::open(&space, "parity-space").unwrap();
        let scan = ds.scan(now).unwrap();
        assert!(scan.rejected.is_empty(), "{:?}", scan.rejected);
        assert_eq!(scan.entries.len(), 1);
        let d = &scan.entries[0];
        assert_eq!(d.id, writer);
        assert_eq!(d.intent, intent);
        assert_eq!(
            d.dirty,
            vec!["src/a.rs".to_string(), "docs/plan.md".to_string()]
        );
        assert_eq!(d.dirty_dropped, 0);
        assert_eq!(d.backend.as_deref(), Some("guest"));
        assert!(
            d.root.as_deref().is_some_and(super::scan::valid_abs_path),
            "declared root is a grammar-valid absolute path: {:?}",
            d.root
        );
        assert!(!d.stale);

        let body = "heads up: coordination/mod.rs is mid-carve; land after #560.";
        let mid = run_snippet(
            py,
            &script,
            tmp.path(),
            &["message", space_arg, &writer, body, "s-native-7f2a", "3600"],
        );
        let ms = MessageSpace::open(&space, "parity-space").unwrap();
        let scan = ms.scan_meta(now).unwrap();
        assert!(scan.rejected.is_empty(), "{:?}", scan.rejected);
        assert_eq!(scan.entries.len(), 1);
        let m = &scan.entries[0];
        assert_eq!(m.id, mid);
        assert_eq!(m.writer, writer);
        assert_eq!(m.to.as_deref(), Some("s-native-7f2a"));
        assert_eq!(m.ttl_s, 3600);
        assert!(!m.expired);
        let full = ms
            .read(&writer, &mid, now)
            .unwrap()
            .expect("message reads back");
        assert_eq!(full.body, body);

        // Broadcast + snippet-default TTL lane.
        let mid2 = run_snippet(
            py,
            &script,
            tmp.path(),
            &["message", space_arg, &writer, "broadcast note"],
        );
        let scan = ms.scan_meta(super::now_ms()).unwrap();
        assert!(scan.rejected.is_empty(), "{:?}", scan.rejected);
        let m2 = scan
            .entries
            .iter()
            .find(|m| m.id == mid2)
            .expect("broadcast listed");
        assert_eq!(m2.to, None);
        assert_eq!(m2.ttl_s, MESSAGE_TTL_DEFAULT_S);
    }
}
