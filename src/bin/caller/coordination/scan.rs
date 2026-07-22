//! Shared liveness-scan machinery and field validators (Track C, C1).
//!
//! The checkpoint kind keeps its shipped hard-error scan (a corrupt
//! space must not half-restore). Liveness kinds (`sessions/`,
//! `messages/`) use the ruled rule-5 amendment instead: per-entry
//! rejections are collected BY NAME with a §1.11 reason token and the
//! scan continues — one malformed same-UID file must not blind the
//! radar. Scan-bound overflow stays corruption-grade (hard error), and
//! nothing is ever skipped silently.

use std::path::{Path, PathBuf};

use super::{io_err, CoordinationError, MAX_SCAN_ENTRIES};

/// §1.11 reason tokens — the named-rejection vocabulary tests pin.
pub(crate) const REJECT_NOT_UTF8: &str = "not-utf8";
pub(crate) const REJECT_MISSING_FRONTMATTER: &str = "missing-frontmatter";
pub(crate) const REJECT_UNTERMINATED_FRONTMATTER: &str = "unterminated-frontmatter";
pub(crate) const REJECT_VERSION_NEWER: &str = "version-newer";
pub(crate) const REJECT_KIND_MISMATCH: &str = "kind-mismatch";
pub(crate) const REJECT_ID_FILENAME_MISMATCH: &str = "id-filename-mismatch";
pub(crate) const REJECT_GRAMMAR: &str = "grammar-violation";
pub(crate) const REJECT_FOREIGN_ENTRY: &str = "foreign-entry";
pub(crate) const REJECT_NOT_REGULAR: &str = "not-regular-file";
pub(crate) const REJECT_FOREIGN_OWNER: &str = "foreign-owner";
pub(crate) const REJECT_OVERSIZE_DOC: &str = "oversize-doc";

/// One surfaced-by-name rejection from a liveness scan.
#[derive(Debug, Clone)]
pub(crate) struct ScanReject {
    pub name: String,
    pub reason: &'static str,
}

/// A liveness scan's outcome: parsed entries plus the loud residue.
#[derive(Debug)]
pub(crate) struct LivenessScan<T> {
    pub entries: Vec<T>,
    pub rejected: Vec<ScanReject>,
}

/// One accepted directory entry: the grammar-valid stem, its path, and
/// the lstat the acceptance checks already ran (callers reuse it for
/// mtime math instead of re-statting).
#[derive(Debug)]
pub(crate) struct ScanEntry {
    pub stem: String,
    pub path: PathBuf,
    pub meta: std::fs::Metadata,
}

/// Enumerate a liveness kind directory: dot-entries skipped (in-flight
/// temp files), `<grammar-id>.md` regular files accepted, everything
/// else rejected by name; the scan-entry bound stays a hard error. A
/// missing directory is an empty space, not an error.
pub(crate) fn scan_liveness_dir(
    dir: &Path,
) -> Result<(Vec<ScanEntry>, Vec<ScanReject>), CoordinationError> {
    let mut found = Vec::new();
    let mut rejected = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((found, rejected)),
        Err(e) => return Err(io_err(e)),
    };
    for (n, entry) in entries.enumerate() {
        if n >= MAX_SCAN_ENTRIES {
            return Err(CoordinationError::ReadRefused(format!(
                "{}: exceeds the {MAX_SCAN_ENTRIES}-entry scan bound",
                dir.display()
            )));
        }
        let entry = entry.map_err(io_err)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // in-flight temp files (atomic-write protocol)
        }
        let Some(stem) = name.strip_suffix(".md") else {
            rejected.push(ScanReject {
                name,
                reason: REJECT_FOREIGN_ENTRY,
            });
            continue;
        };
        if super::sanitize_key(stem) != stem {
            rejected.push(ScanReject {
                name,
                reason: REJECT_GRAMMAR,
            });
            continue;
        }
        // lstat — symlinks and non-regular files surface by name and
        // are never opened.
        let meta = match std::fs::symlink_metadata(entry.path()) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue, // GC race: absent
            Err(e) => return Err(io_err(e)),
        };
        if !meta.is_file() {
            rejected.push(ScanReject {
                name,
                reason: REJECT_NOT_REGULAR,
            });
            continue;
        }
        if !owned_by_current_user(&meta) {
            rejected.push(ScanReject {
                name,
                reason: REJECT_FOREIGN_OWNER,
            });
            continue;
        }
        found.push(ScanEntry {
            stem: stem.to_string(),
            path: entry.path(),
            meta,
        });
    }
    found.sort_by(|a, b| a.stem.cmp(&b.stem));
    Ok((found, rejected))
}

/// Defensive open for liveness scans: same §9 rule-5 posture as the
/// checkpoint kind, but per-entry outcomes are data (the rule-5
/// liveness amendment) — a raced-away file is absence, a bad file is a
/// named rejection, and only real I/O trouble is an error.
pub(crate) enum DefensiveRead {
    Ok(Vec<u8>),
    Vanished,
    Reject(&'static str),
}

pub(crate) fn open_liveness(path: &Path) -> Result<DefensiveRead, CoordinationError> {
    use std::io::Read;
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = match opts.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DefensiveRead::Vanished),
        // O_NOFOLLOW on a symlink → ELOOP (surfaces as FilesystemLoop
        // or a raw os error depending on platform): a named rejection.
        Err(e) if e.raw_os_error() == Some(libc_eloop()) => {
            return Ok(DefensiveRead::Reject(REJECT_NOT_REGULAR))
        }
        Err(e) => return Err(io_err(e)),
    };
    let meta = f.metadata().map_err(io_err)?;
    if !meta.is_file() {
        return Ok(DefensiveRead::Reject(REJECT_NOT_REGULAR));
    }
    if meta.len() as usize > super::MAX_DOC_BYTES {
        return Ok(DefensiveRead::Reject(REJECT_OVERSIZE_DOC));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    f.read_to_end(&mut bytes).map_err(io_err)?;
    Ok(DefensiveRead::Ok(bytes))
}

#[cfg(unix)]
fn libc_eloop() -> i32 {
    libc::ELOOP
}
#[cfg(not(unix))]
fn libc_eloop() -> i32 {
    // No O_NOFOLLOW on this path; the is_file() check below is the
    // non-regular gate. Value never matches a real Windows error here.
    -1
}

/// Same-UID ownership check. Windows has no uid; profile ACLs are the
/// boundary there (same posture as the rest of `~/.intendant`).
#[cfg(unix)]
pub(crate) fn owned_by_current_user(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY-free: geteuid is a pure syscall wrapper with no
    // preconditions; libc marks it unsafe only as FFI.
    meta.uid() == unsafe { libc::geteuid() }
}
#[cfg(not(unix))]
pub(crate) fn owned_by_current_user(_meta: &std::fs::Metadata) -> bool {
    true
}

/// A parsed protocol document: raw frontmatter fields + body. Fields
/// are machine values only (validated by each kind); free text lives
/// only in the body, and everything is DATA for the reader to weigh.
#[derive(Debug)]
pub(crate) struct RawDoc {
    fields: Vec<(String, String)>,
    pub body: String,
}

impl RawDoc {
    pub(crate) fn field(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Parse the versioned fence + line-oriented `key: value` frontmatter
/// (deliberately not YAML — same shape the checkpoint kind ships).
/// Returns a §1.11 reason token on refusal; `expected_kinds` pins the
/// document kind (a set — `messages/` holds both `message` and
/// `radar-note`) and `id` must equal the filename stem.
pub(crate) fn parse_doc(
    bytes: Vec<u8>,
    stem: &str,
    expected_kinds: &[&str],
) -> Result<RawDoc, &'static str> {
    let Ok(text) = String::from_utf8(bytes) else {
        return Err(REJECT_NOT_UTF8);
    };
    let Some(rest) = text.strip_prefix("---\n") else {
        return Err(REJECT_MISSING_FRONTMATTER);
    };
    let Some((front, body)) = rest.split_once("\n---\n") else {
        return Err(REJECT_UNTERMINATED_FRONTMATTER);
    };
    let mut fields = Vec::new();
    for line in front.lines() {
        if let Some((k, v)) = line.split_once(": ") {
            fields.push((k.to_string(), v.trim().to_string()));
        }
    }
    let doc = RawDoc {
        fields,
        body: body.trim_end_matches('\n').to_string(),
    };
    match doc.field("v").and_then(|v| v.parse::<u32>().ok()) {
        Some(v) if v <= 1 => {}
        _ => return Err(REJECT_VERSION_NEWER),
    }
    if !doc
        .field("kind")
        .is_some_and(|k| expected_kinds.contains(&k))
    {
        return Err(REJECT_KIND_MISMATCH);
    }
    if doc.field("id") != Some(stem) {
        return Err(REJECT_ID_FILENAME_MISMATCH);
    }
    Ok(doc)
}

/// Repo-relative path grammar (§2.3 of the ruled protocol): the only
/// charset that may reach a summary line, structurally repo-relative.
pub(crate) fn valid_rel_path(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 512
        && !s.starts_with('/')
        && !s.starts_with('-')
        && s.split('/').all(|seg| !seg.is_empty() && seg != "..")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
}

/// Absolute box-local path field (`root:`): printable, bounded, no
/// control bytes — machine value, never rendered into summaries.
/// "Absolute" is judged for every platform's spelling (all three are
/// first-class): unix `/…`, Windows drive (`C:\…` / `C:/…`) and UNC
/// (`\\server\…`) forms — a Windows daemon's declarations must carry
/// their roots too.
pub(crate) fn valid_abs_path(s: &str) -> bool {
    let bytes = s.as_bytes();
    let windows_abs = matches!(bytes, [drive, b':', b'\\' | b'/', ..] if drive.is_ascii_alphabetic())
        || s.starts_with("\\\\");
    !s.is_empty()
        && s.len() <= 1024
        && (s.starts_with('/') || windows_abs)
        && !bytes.iter().any(|b| b.is_ascii_control())
}

/// Closed backend enum for declaration frontmatter.
pub(crate) fn valid_backend(s: &str) -> bool {
    matches!(
        s,
        "native" | "codex" | "claude-code" | "kimi" | "pi" | "guest"
    )
}

/// Future timestamps are hints, never trusted: clamp to `now` before
/// any TTL math (§9).
pub(crate) fn effective_mtime_ms(meta: &std::fs::Metadata, now_ms: u64) -> u64 {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    mtime.min(now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_path_grammar_rejects_hostile_names() {
        assert!(valid_rel_path("src/bin/caller/tools.rs"));
        assert!(valid_rel_path("a-b_c.d/e"));
        for bad in [
            "",
            "/abs/path",
            "-leading-dash",
            "has space",
            "has\nnewline",
            "has\x1b[31mansi",
            "dot/../escape",
            "..",
            "trailing//empty",
            "unicode\u{202e}rtl",
        ] {
            assert!(!valid_rel_path(bad), "{bad:?} must be rejected");
        }
        assert!(!valid_rel_path(&"x".repeat(513)), "length bound");
    }

    #[test]
    fn abs_path_grammar_accepts_every_platform_spelling() {
        for good in [
            "/Users/u/projects/x",
            "/tmp",
            "C:\\Users\\ci\\repo",
            "c:/work/repo",
            "\\\\server\\share\\repo",
        ] {
            assert!(valid_abs_path(good), "{good:?} must be accepted");
        }
        for bad in [
            "",
            "relative/path",
            "C:no-separator",
            "1:\\not-a-drive",
            "has\x1bcontrol/",
            &format!("/{}", "x".repeat(1024)),
        ] {
            assert!(!valid_abs_path(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn parse_doc_names_each_rejection() {
        const M: &[&str] = &["message", "radar-note"];
        let ok = b"---\nv: 1\nkind: message\nid: abc\n---\nbody\n".to_vec();
        assert!(parse_doc(ok, "abc", M).is_ok());
        let note = b"---\nv: 1\nkind: radar-note\nid: abc\n---\nbody\n".to_vec();
        assert!(parse_doc(note, "abc", M).is_ok(), "kind set, not scalar");
        assert_eq!(
            parse_doc(vec![0xff, 0xfe], "a", M).unwrap_err(),
            REJECT_NOT_UTF8
        );
        assert_eq!(
            parse_doc(b"no fence".to_vec(), "a", M).unwrap_err(),
            REJECT_MISSING_FRONTMATTER
        );
        assert_eq!(
            parse_doc(b"---\nv: 1\nnever closed".to_vec(), "a", M).unwrap_err(),
            REJECT_UNTERMINATED_FRONTMATTER
        );
        assert_eq!(
            parse_doc(
                b"---\nv: 2\nkind: message\nid: a\n---\nb\n".to_vec(),
                "a",
                M
            )
            .unwrap_err(),
            REJECT_VERSION_NEWER
        );
        assert_eq!(
            parse_doc(
                b"---\nv: 1\nkind: session-declaration\nid: a\n---\nb\n".to_vec(),
                "a",
                M
            )
            .unwrap_err(),
            REJECT_KIND_MISMATCH
        );
        assert_eq!(
            parse_doc(
                b"---\nv: 1\nkind: message\nid: other\n---\nb\n".to_vec(),
                "a",
                M
            )
            .unwrap_err(),
            REJECT_ID_FILENAME_MISMATCH
        );
    }

    #[test]
    fn liveness_scan_collects_rejections_and_continues() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("good-one.md"), "x").unwrap();
        std::fs::write(dir.join(".tmp-inflight"), "x").unwrap();
        std::fs::write(dir.join("stray.txt"), "x").unwrap();
        std::fs::write(dir.join("Bad Name.md"), "x").unwrap();
        std::fs::create_dir(dir.join("subdir.md")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.path().join("elsewhere"), dir.join("link-x.md")).unwrap();

        let (found, rejected) = scan_liveness_dir(&dir).unwrap();
        assert_eq!(found.len(), 1, "only the conforming entry parses");
        assert_eq!(found[0].stem, "good-one");
        let reasons: Vec<&str> = rejected.iter().map(|r| r.reason).collect();
        assert!(reasons.contains(&REJECT_FOREIGN_ENTRY), "{reasons:?}");
        assert!(reasons.contains(&REJECT_GRAMMAR), "{reasons:?}");
        assert!(reasons.contains(&REJECT_NOT_REGULAR), "{reasons:?}");
        #[cfg(unix)]
        assert_eq!(rejected.len(), 4, "symlink also surfaced: {rejected:?}");
    }

    #[test]
    fn open_liveness_outcomes_are_data() {
        let tmp = tempfile::tempdir().unwrap();
        let ok = tmp.path().join("ok.md");
        std::fs::write(&ok, "hello").unwrap();
        assert!(matches!(
            open_liveness(&ok).unwrap(),
            DefensiveRead::Ok(b) if b == b"hello"
        ));
        assert!(matches!(
            open_liveness(&tmp.path().join("absent.md")).unwrap(),
            DefensiveRead::Vanished
        ));
        let big = tmp.path().join("big.md");
        std::fs::write(&big, vec![b'x'; super::super::MAX_DOC_BYTES + 1]).unwrap();
        assert!(matches!(
            open_liveness(&big).unwrap(),
            DefensiveRead::Reject(REJECT_OVERSIZE_DOC)
        ));
        #[cfg(unix)]
        {
            let link = tmp.path().join("link.md");
            std::os::unix::fs::symlink(&ok, &link).unwrap();
            assert!(matches!(
                open_liveness(&link).unwrap(),
                DefensiveRead::Reject(REJECT_NOT_REGULAR)
            ));
        }
    }

    #[test]
    fn missing_dir_is_an_empty_space() {
        let tmp = tempfile::tempdir().unwrap();
        let (found, rejected) = scan_liveness_dir(&tmp.path().join("absent")).unwrap();
        assert!(found.is_empty() && rejected.is_empty());
    }

    #[test]
    fn scan_bound_overflow_is_corruption_grade() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("messages-writer");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..=MAX_SCAN_ENTRIES {
            std::fs::write(dir.join(format!("m{i}.md")), "x").unwrap();
        }
        let err = scan_liveness_dir(&dir).unwrap_err();
        assert!(err.to_string().contains("scan bound"), "{err}");
    }

    #[test]
    fn future_mtimes_are_clamped() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f");
        std::fs::write(&p, "x").unwrap();
        let meta = std::fs::metadata(&p).unwrap();
        // A "now" far in the past makes the real mtime a future
        // timestamp — the clamp must cap it to now.
        assert_eq!(effective_mtime_ms(&meta, 1000), 1000);
    }
}
