//! Session-declaration kind (Track C, C1): the `sessions/` liveness
//! lane. Each live session keeps exactly one declaration — who it is,
//! where it works, what it intends, which paths it believes it is
//! dirtying — refreshed by mtime touch. Declarations are liveness DATA
//! for the collision radar, never authority: staleness is advisory
//! (45 min), garbage collection is time-based (24 h — the ruled §9
//! rule-8 amendment for liveness kinds), and a reader treats every
//! field as unverified same-UID input.

use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use super::scan::{
    self, DefensiveRead, LivenessScan, ScanReject, REJECT_GRAMMAR, REJECT_NOT_REGULAR,
};
use super::{io_err, restrict_dir_modes, sanitize_key, CoordinationError};

pub(crate) const KIND_SESSION_DECLARATION: &str = "session-declaration";
/// Live-declaration cap per space (write-side rule 6).
pub(crate) const MAX_DECLARATIONS_PER_SPACE: usize = 256;
/// Dirty-path lines accepted per declaration.
pub(crate) const MAX_DIRTY_PATHS: usize = 64;
/// Radar staleness threshold: older than this ⇒ flagged, still shown.
pub(crate) const DECLARATION_STALE_MS: u64 = 45 * 60 * 1000;
/// GC threshold: valid declarations older than this are removed.
pub(crate) const DECLARATION_GC_MS: u64 = 24 * 60 * 60 * 1000;

/// Writer-side input. All references — the glue owns the strings.
pub(crate) struct DeclarationInput<'a> {
    pub id: &'a str,
    pub session: Option<&'a str>,
    pub backend: Option<&'a str>,
    pub root: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub intent: &'a str,
    pub dirty: &'a [String],
}

/// The parsed view (DATA for the radar to weigh).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SessionDeclaration {
    pub id: String,
    pub session: Option<String>,
    pub backend: Option<String>,
    pub root: Option<String>,
    pub branch: Option<String>,
    pub created_ms: u64,
    pub intent: String,
    pub dirty: Vec<String>,
    /// Dirty lines beyond the parse cap or outside the grammar —
    /// counted, never rendered.
    pub dirty_dropped: usize,
    /// Clamped mtime (future timestamps capped to now).
    pub effective_mtime_ms: u64,
    pub stale: bool,
}

/// The `sessions/` store for one coordination space. Constructed from
/// a RESOLVED space dir (see `paths::resolve_space_dir`) — new kinds
/// take the seam's output, they do not re-derive it.
pub(crate) struct DeclarationSpace {
    dir: PathBuf,
    space: String,
}

impl DeclarationSpace {
    pub(crate) fn open(space_dir: &Path, space: &str) -> Result<Self, CoordinationError> {
        let dir = space_dir.join("sessions");
        std::fs::create_dir_all(&dir).map_err(io_err)?;
        restrict_dir_modes(&dir)?;
        Ok(DeclarationSpace {
            dir,
            space: space.to_string(),
        })
    }

    /// Create-or-replace this session's own declaration (atomic,
    /// bounded, 0600). `created_ms` survives rewrites of an existing
    /// declaration; the per-space cap gates only NEW declarations.
    pub(crate) fn write_own(
        &self,
        input: &DeclarationInput<'_>,
    ) -> Result<SessionDeclaration, CoordinationError> {
        let id = input.id;
        if id.is_empty() || sanitize_key(id) != id {
            return Err(CoordinationError::WriteRefused(format!(
                "declaration id {id:?} is outside the filename grammar"
            )));
        }
        if input.intent.trim().is_empty() {
            return Err(CoordinationError::WriteRefused(
                "declaration intent must be non-empty".into(),
            ));
        }
        if let Some(b) = input.backend {
            if !scan::valid_backend(b) {
                return Err(CoordinationError::WriteRefused(format!(
                    "backend {b:?} is outside the closed set"
                )));
            }
        }
        if let Some(r) = input.root {
            if !scan::valid_abs_path(r) {
                return Err(CoordinationError::WriteRefused(format!(
                    "root {r:?} is not a bounded absolute path"
                )));
            }
        }
        if let Some(b) = input.branch {
            if !valid_branch(b) {
                return Err(CoordinationError::WriteRefused(format!(
                    "branch {b:?} is outside the branch grammar"
                )));
            }
        }
        if input.dirty.len() > MAX_DIRTY_PATHS {
            return Err(CoordinationError::WriteRefused(format!(
                "{} dirty paths exceeds the {MAX_DIRTY_PATHS} cap — truncate at the caller",
                input.dirty.len()
            )));
        }
        for p in input.dirty {
            if !scan::valid_rel_path(p) {
                return Err(CoordinationError::WriteRefused(format!(
                    "dirty path {p:?} is outside the repo-relative grammar"
                )));
            }
        }

        let path = self.dir.join(format!("{id}.md"));
        let now = super::now_ms();
        // Rewrite keeps the original created_ms; a new declaration is
        // gated by the per-space cap.
        let created_ms = match self.read_own(id)? {
            Some(existing) => existing.created_ms,
            None => {
                let (live, _) = scan::scan_liveness_dir(&self.dir)?;
                if live.len() >= MAX_DECLARATIONS_PER_SPACE {
                    return Err(CoordinationError::WriteRefused(format!(
                        "space holds {} declarations; the bound is {MAX_DECLARATIONS_PER_SPACE}",
                        live.len()
                    )));
                }
                now
            }
        };

        let mut doc = String::new();
        doc.push_str("---\n");
        doc.push_str("v: 1\n");
        doc.push_str(&format!("kind: {KIND_SESSION_DECLARATION}\n"));
        doc.push_str(&format!("id: {id}\n"));
        doc.push_str(&format!("space: {}\n", self.space));
        if let Some(s) = input.session {
            doc.push_str(&format!("session: {}\n", sanitize_key(s)));
        }
        if let Some(b) = input.backend {
            doc.push_str(&format!("backend: {b}\n"));
        }
        if let Some(r) = input.root {
            doc.push_str(&format!("root: {r}\n"));
        }
        if let Some(b) = input.branch {
            doc.push_str(&format!("branch: {b}\n"));
        }
        doc.push_str(&format!("created_ms: {created_ms}\n"));
        doc.push_str("attribution: unverified-same-uid\n");
        doc.push_str("---\n");
        doc.push_str("## intent\n");
        doc.push_str(input.intent.trim());
        doc.push('\n');
        if !input.dirty.is_empty() {
            doc.push_str("\n## dirty\n");
            for p in input.dirty {
                doc.push_str(&format!("- {p}\n"));
            }
        }
        if doc.len() > super::MAX_DOC_BYTES {
            return Err(CoordinationError::WriteRefused(format!(
                "declaration document is {} bytes; the §9 bound is {}",
                doc.len(),
                super::MAX_DOC_BYTES
            )));
        }

        let tmp = self.dir.join(format!(".{id}.tmp"));
        {
            let mut f = std::fs::File::create(&tmp).map_err(io_err)?;
            f.write_all(doc.as_bytes()).map_err(io_err)?;
            f.sync_all().map_err(io_err)?;
        }
        super::restrict_file_modes(&tmp)?;
        std::fs::rename(&tmp, &path).map_err(io_err)?;

        Ok(parse_declaration(
            id,
            doc.into_bytes(),
            std::fs::symlink_metadata(&path).map_err(io_err)?,
            now,
        )
        .expect("just-written declaration must parse"))
    }

    /// Heartbeat: bump the own declaration's mtime. `Ok(false)` means
    /// the file is gone (GC'd or never written) — re-declare.
    pub(crate) fn touch_own(&self, id: &str) -> Result<bool, CoordinationError> {
        if id.is_empty() || sanitize_key(id) != id {
            return Err(CoordinationError::WriteRefused(format!(
                "declaration id {id:?} is outside the filename grammar"
            )));
        }
        let path = self.dir.join(format!("{id}.md"));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        let f = match opts.open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(io_err(e)),
        };
        let meta = f.metadata().map_err(io_err)?;
        if !meta.is_file() {
            return Err(CoordinationError::WriteRefused(format!(
                "{}: not a regular file",
                path.display()
            )));
        }
        f.set_modified(std::time::SystemTime::now())
            .map_err(io_err)?;
        Ok(true)
    }

    /// Remove the own declaration on clean exit. Absence is fine.
    pub(crate) fn remove_own(&self, id: &str) -> Result<bool, CoordinationError> {
        if id.is_empty() || sanitize_key(id) != id {
            return Err(CoordinationError::WriteRefused(format!(
                "declaration id {id:?} is outside the filename grammar"
            )));
        }
        match std::fs::remove_file(self.dir.join(format!("{id}.md"))) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_err(e)),
        }
    }

    /// Read one declaration by id (own-read path for rewrite
    /// semantics; also usable by consumers).
    pub(crate) fn read_own(
        &self,
        id: &str,
    ) -> Result<Option<SessionDeclaration>, CoordinationError> {
        let path = self.dir.join(format!("{id}.md"));
        let now = super::now_ms();
        match scan::open_liveness(&path)? {
            DefensiveRead::Ok(bytes) => {
                let meta = match std::fs::symlink_metadata(&path) {
                    Ok(m) => m,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(io_err(e)),
                };
                Ok(parse_declaration(id, bytes, meta, now).ok())
            }
            DefensiveRead::Vanished => Ok(None),
            DefensiveRead::Reject(_) => Ok(None),
        }
    }

    /// The radar's read: every declaration in the space, with the
    /// rule-5 liveness amendment — malformed entries surface by name.
    #[cfg_attr(not(test), allow(dead_code))] // C2 radar's entry point; write/GC paths are live (C1 PR B).
    pub(crate) fn scan(
        &self,
        now_ms: u64,
    ) -> Result<LivenessScan<SessionDeclaration>, CoordinationError> {
        scan_dir(&self.dir, now_ms)
    }
}

/// Directory-level scan, shared with GC (which must not create dirs).
pub(crate) fn scan_dir(
    dir: &Path,
    now_ms: u64,
) -> Result<LivenessScan<SessionDeclaration>, CoordinationError> {
    let (found, mut rejected) = scan::scan_liveness_dir(dir)?;
    let mut entries = Vec::with_capacity(found.len());
    for entry in found {
        let name = format!("{}.md", entry.stem);
        let bytes = match scan::open_liveness(&entry.path)? {
            DefensiveRead::Ok(bytes) => bytes,
            DefensiveRead::Vanished => continue,
            DefensiveRead::Reject(reason) => {
                rejected.push(ScanReject { name, reason });
                continue;
            }
        };
        match parse_declaration(&entry.stem, bytes, entry.meta, now_ms) {
            Ok(decl) => entries.push(decl),
            Err(reason) => rejected.push(ScanReject { name, reason }),
        }
    }
    Ok(LivenessScan { entries, rejected })
}

/// Branch names render in radar summaries: printable, bounded, no
/// control bytes, no leading dash (option-injection hygiene).
/// `pub(crate)`: the lifecycle glue pre-filters its optional branch
/// hint through the same grammar rather than sinking a declaration on
/// an exotic branch name.
pub(crate) fn valid_branch(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && !s.starts_with('-')
        && !s.bytes().any(|b| b.is_ascii_control())
}

fn parse_declaration(
    stem: &str,
    bytes: Vec<u8>,
    meta: std::fs::Metadata,
    now_ms: u64,
) -> Result<SessionDeclaration, &'static str> {
    let doc = scan::parse_doc(bytes, stem, &[KIND_SESSION_DECLARATION])?;
    let backend = doc.field("backend").map(str::to_string);
    if let Some(b) = &backend {
        if !scan::valid_backend(b) {
            return Err(REJECT_GRAMMAR);
        }
    }
    let root = doc.field("root").map(str::to_string);
    if let Some(r) = &root {
        if !scan::valid_abs_path(r) {
            return Err(REJECT_GRAMMAR);
        }
    }
    let branch = doc.field("branch").map(str::to_string);
    if let Some(b) = &branch {
        if !valid_branch(b) {
            return Err(REJECT_GRAMMAR);
        }
    }
    let Some(created_ms) = doc.field("created_ms").and_then(|v| v.parse::<u64>().ok()) else {
        return Err(REJECT_GRAMMAR);
    };
    if !meta.is_file() {
        return Err(REJECT_NOT_REGULAR);
    }

    // Body sections: `## intent` free text, then optional `## dirty`
    // with `- <path>` lines. Section heads match exactly — body text
    // is data, so a stray heading merely truncates the writer's OWN
    // intent, and every dirty line re-passes the grammar.
    let mut intent = String::new();
    let mut dirty = Vec::new();
    let mut dirty_dropped = 0usize;
    let mut section = "";
    for line in doc.body.lines() {
        match line {
            "## intent" => section = "intent",
            "## dirty" => section = "dirty",
            _ => match section {
                "intent" => {
                    if !intent.is_empty() {
                        intent.push('\n');
                    }
                    intent.push_str(line);
                }
                "dirty" => {
                    let Some(p) = line.strip_prefix("- ") else {
                        if !line.trim().is_empty() {
                            dirty_dropped += 1;
                        }
                        continue;
                    };
                    if dirty.len() >= MAX_DIRTY_PATHS || !scan::valid_rel_path(p) {
                        dirty_dropped += 1;
                    } else {
                        dirty.push(p.to_string());
                    }
                }
                _ => {}
            },
        }
    }
    let intent = intent.trim().to_string();
    if intent.is_empty() {
        return Err(REJECT_GRAMMAR);
    }

    let effective_mtime_ms = scan::effective_mtime_ms(&meta, now_ms);
    Ok(SessionDeclaration {
        id: stem.to_string(),
        session: doc.field("session").map(str::to_string),
        backend,
        root,
        branch,
        created_ms: created_ms.min(now_ms),
        intent,
        dirty,
        dirty_dropped,
        effective_mtime_ms,
        stale: now_ms.saturating_sub(effective_mtime_ms) > DECLARATION_STALE_MS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(tmp: &tempfile::TempDir) -> DeclarationSpace {
        DeclarationSpace::open(&tmp.path().join("space"), "test-space").unwrap()
    }

    fn input<'a>(id: &'a str, intent: &'a str, dirty: &'a [String]) -> DeclarationInput<'a> {
        DeclarationInput {
            id,
            session: Some("sess-1"),
            backend: Some("native"),
            root: Some("/tmp/proj"),
            branch: Some("feat/x"),
            intent,
            dirty,
        }
    }

    #[test]
    fn declare_touch_rescan_remove_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let ds = space(&tmp);
        let dirty = vec!["src/a.rs".to_string(), "docs/b.md".to_string()];
        let d = ds
            .write_own(&input("s-alpha", "carve the bus module", &dirty))
            .unwrap();
        assert_eq!(d.dirty, dirty);
        assert!(!d.stale);

        let scan = ds.scan(super::super::now_ms()).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert!(scan.rejected.is_empty());
        assert_eq!(scan.entries[0].intent, "carve the bus module");
        assert_eq!(scan.entries[0].backend.as_deref(), Some("native"));

        assert!(ds.touch_own("s-alpha").unwrap());
        assert!(
            !ds.touch_own("s-ghost").unwrap(),
            "absent → re-declare signal"
        );

        // Rewrite preserves created_ms.
        let again = ds.write_own(&input("s-alpha", "new phase", &[])).unwrap();
        assert_eq!(again.created_ms, d.created_ms);

        assert!(ds.remove_own("s-alpha").unwrap());
        assert!(!ds.remove_own("s-alpha").unwrap());
        assert!(ds.scan(super::super::now_ms()).unwrap().entries.is_empty());
    }

    #[test]
    fn stale_flag_reflects_mtime_age() {
        let tmp = tempfile::tempdir().unwrap();
        let ds = space(&tmp);
        ds.write_own(&input("s-old", "long ago", &[])).unwrap();
        let later = super::super::now_ms() + DECLARATION_STALE_MS + 1000;
        let scan = ds.scan(later).unwrap();
        assert!(scan.entries[0].stale);
    }

    #[test]
    fn write_refusals_are_named() {
        let tmp = tempfile::tempdir().unwrap();
        let ds = space(&tmp);
        let bad_dirty = vec!["../escape".to_string()];
        for (inp, needle) in [
            (input("Bad Id", "x", &[]), "grammar"),
            (input("s-a", "   ", &[]), "non-empty"),
            (input("s-a", "x", &bad_dirty), "repo-relative"),
        ] {
            let err = ds.write_own(&inp).unwrap_err().to_string();
            assert!(err.contains(needle), "{err}");
        }
        let mut inp = input("s-a", "x", &[]);
        inp.backend = Some("mystery");
        assert!(ds
            .write_own(&inp)
            .unwrap_err()
            .to_string()
            .contains("closed set"));
        let too_many: Vec<String> = (0..=MAX_DIRTY_PATHS).map(|i| format!("f{i}")).collect();
        let err = ds
            .write_own(&input("s-a", "x", &too_many))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cap"), "{err}");
    }

    #[test]
    fn malformed_neighbor_never_blinds_the_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let ds = space(&tmp);
        ds.write_own(&input("s-good", "fine", &[])).unwrap();
        let dir = tmp.path().join("space/sessions");
        std::fs::write(dir.join("s-evil.md"), "not a document at all").unwrap();
        std::fs::write(
            dir.join("s-hostile.md"),
            "---\nv: 1\nkind: session-declaration\nid: s-hostile\nbranch: -rf\ncreated_ms: 1\n---\n## intent\nx\n",
        )
        .unwrap();
        let scan = ds.scan(super::super::now_ms()).unwrap();
        assert_eq!(scan.entries.len(), 1, "good entry survives");
        assert_eq!(scan.rejected.len(), 2, "{:?}", scan.rejected);
        assert!(scan.rejected.iter().all(|r| !r.name.is_empty()));
    }

    #[test]
    fn adversarial_dirty_lines_are_counted_not_rendered() {
        let tmp = tempfile::tempdir().unwrap();
        let ds = space(&tmp);
        let dir = tmp.path().join("space/sessions");
        std::fs::write(
            dir.join("s-sly.md"),
            "---\nv: 1\nkind: session-declaration\nid: s-sly\ncreated_ms: 1\n---\n\
             ## intent\nlooks fine\n\n## dirty\n- src/ok.rs\n- ../../etc/passwd\n- \u{1b}[31mred\n- --flag\n",
        )
        .unwrap();
        let scan = ds.scan(super::super::now_ms()).unwrap();
        assert_eq!(scan.entries.len(), 1);
        let d = &scan.entries[0];
        assert_eq!(d.dirty, vec!["src/ok.rs".to_string()]);
        assert_eq!(d.dirty_dropped, 3, "hostile paths counted, never kept");
    }

    #[test]
    fn declaration_cap_gates_new_but_not_rewrites() {
        let tmp = tempfile::tempdir().unwrap();
        let ds = space(&tmp);
        for i in 0..MAX_DECLARATIONS_PER_SPACE {
            ds.write_own(&input(&format!("s-{i}"), "x", &[])).unwrap();
        }
        let err = ds.write_own(&input("s-overflow", "x", &[])).unwrap_err();
        assert!(err.to_string().contains("bound"), "{err}");
        ds.write_own(&input("s-0", "rewrite fine", &[])).unwrap();
    }
}
