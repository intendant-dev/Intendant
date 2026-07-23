//! Message kind (Track C, C1): the `messages/<writer>/` async mail
//! lane. A writer leaves bounded, TTL'd notes for the space (or a
//! named recipient); readers poll. Delivery is best-effort liveness
//! data — never an instruction channel: bodies are DATA the reader
//! weighs, and expiry is advisory until GC removes the file. The
//! `daemon` writer name is reserved for the daemon's own lanes
//! (C2 radar notes); plain writers are refused it here.
#![cfg_attr(not(test), allow(dead_code))] // C1 staging: GC already consumes the scan side; the write/read lanes are C2 (radar notes) / C3 (messages + skill). Drop as that wiring lands.

use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use super::scan::{
    self, DefensiveRead, LivenessScan, ReadBudget, ScanReject, REJECT_FOREIGN_ENTRY,
    REJECT_READ_BUDGET,
};
use super::{io_err, restrict_dir_modes, sanitize_key, CoordinationError};

pub(crate) const KIND_MESSAGE: &str = "message";
pub(crate) const KIND_RADAR_NOTE: &str = "radar-note";
/// Both kinds a `messages/` scan accepts.
const MESSAGE_KINDS: &[&str] = &[KIND_MESSAGE, KIND_RADAR_NOTE];
/// Live-message cap per writer dir (write-side rule 6).
pub(crate) const MAX_MESSAGES_PER_WRITER: usize = 64;
/// Writer-dir cap per space — more is corruption-grade.
pub(crate) const MAX_WRITER_DIRS: usize = 128;
pub(crate) const MESSAGE_TTL_DEFAULT_S: u32 = 86_400;
pub(crate) const MESSAGE_TTL_MIN_S: u32 = 60;
pub(crate) const MESSAGE_TTL_MAX_S: u32 = 604_800;
/// Reserved for the daemon's C2 lanes.
pub(crate) const RESERVED_DAEMON_WRITER: &str = "daemon";
/// Radar-note spam guard (the §2.8 cooldown precedent applied to the
/// bus lane): at most one radar note lands on a recipient per window,
/// on top of the per-overlap-set dedup by note id.
pub(crate) const RADAR_NOTE_COOLDOWN_MS: u64 = 10 * 60 * 1000;
/// Overlap paths listed in one radar-note body; the remainder is
/// counted in the note's fixed template line (bounds are write-side
/// too — rule 6 — and the §1.5 dirty-cap spirit applies here).
pub(crate) const RADAR_NOTE_MAX_PATHS: usize = 64;

/// Frontmatter-only view (listing/radar); `body` costs a `read`.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct MessageMeta {
    pub id: String,
    pub writer: String,
    pub kind: String,
    pub to: Option<String>,
    pub created_ms: u64,
    pub ttl_s: u32,
    pub expired: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Message {
    #[serde(flatten)]
    pub meta: MessageMeta,
    pub body: String,
}

pub(crate) struct MessageInput<'a> {
    pub to: Option<&'a str>,
    pub ttl_s: Option<u32>,
    pub body: &'a str,
}

/// Input for [`MessageSpace::write_radar_note`] — every field is a
/// machine value the writer re-validates (§2.3): grammar ids, the
/// closed source flags, a u32 PR number, repo-relative paths.
pub(crate) struct RadarNoteInput<'a> {
    /// The flagged party this note is addressed to (`to:`).
    pub to: &'a str,
    /// All flagged parties (`parties:`), recipient included.
    pub parties: &'a [&'a str],
    /// Overlap evidence: declared working sets / observed git status.
    pub declared: bool,
    pub git: bool,
    /// Open-PR overlap, when the counterpart is a PR's file set.
    pub pr: Option<u32>,
    /// The overlapping repo-relative paths (full parse grammar, not
    /// the renderer's display bound — the note body is exact).
    pub paths: &'a [String],
    pub ttl_s: Option<u32>,
}

/// The `messages/` store for one coordination space (resolved dir in,
/// like `DeclarationSpace`).
pub(crate) struct MessageSpace {
    dir: PathBuf,
    space: String,
}

impl MessageSpace {
    pub(crate) fn open(space_dir: &Path, space: &str) -> Result<Self, CoordinationError> {
        let dir = space_dir.join("messages");
        std::fs::create_dir_all(&dir).map_err(io_err)?;
        restrict_dir_modes(&dir)?;
        Ok(MessageSpace {
            dir,
            space: space.to_string(),
        })
    }

    /// Leave one message (atomic, bounded, 0600). TTL hints clamp
    /// silently into `[MIN, MAX]`; caps refuse loudly.
    pub(crate) fn write(
        &self,
        writer: &str,
        input: &MessageInput<'_>,
    ) -> Result<MessageMeta, CoordinationError> {
        if writer.is_empty() || sanitize_key(writer) != writer {
            return Err(CoordinationError::WriteRefused(format!(
                "writer {writer:?} is outside the filename grammar"
            )));
        }
        if writer == RESERVED_DAEMON_WRITER {
            return Err(CoordinationError::WriteRefused(format!(
                "writer {RESERVED_DAEMON_WRITER:?} is reserved for the daemon's lanes"
            )));
        }
        let to = match input.to.map(str::trim).filter(|s| !s.is_empty()) {
            None => None,
            Some(raw) if sanitize_key(raw) == raw => Some(raw.to_string()),
            Some(raw) => {
                return Err(CoordinationError::WriteRefused(format!(
                    "recipient {raw:?} is outside the filename grammar"
                )))
            }
        };
        if input.body.trim().is_empty() {
            return Err(CoordinationError::WriteRefused(
                "message body must be non-empty".into(),
            ));
        }
        let ttl_s = input
            .ttl_s
            .unwrap_or(MESSAGE_TTL_DEFAULT_S)
            .clamp(MESSAGE_TTL_MIN_S, MESSAGE_TTL_MAX_S);

        let writer_dir = self.dir.join(writer);
        if !writer_dir.is_dir() {
            let (dirs, _) = writer_dirs(&self.dir)?;
            if dirs.len() >= MAX_WRITER_DIRS {
                return Err(CoordinationError::WriteRefused(format!(
                    "space holds {} writer dirs; the bound is {MAX_WRITER_DIRS}",
                    dirs.len()
                )));
            }
        }
        std::fs::create_dir_all(&writer_dir).map_err(io_err)?;
        restrict_dir_modes(&writer_dir)?;
        let (live, _) = scan::scan_liveness_dir(&writer_dir)?;
        if live.len() >= MAX_MESSAGES_PER_WRITER {
            return Err(CoordinationError::WriteRefused(format!(
                "writer {writer:?} holds {} messages; the bound is {MAX_MESSAGES_PER_WRITER} — \
                 delete or let expiry GC catch up",
                live.len()
            )));
        }

        let id = format!("m-{}", super::ulid_like());
        let created_ms = super::now_ms();
        let mut doc = String::new();
        doc.push_str("---\n");
        doc.push_str("v: 1\n");
        doc.push_str(&format!("kind: {KIND_MESSAGE}\n"));
        doc.push_str(&format!("id: {id}\n"));
        doc.push_str(&format!("space: {}\n", self.space));
        doc.push_str(&format!("from: {writer}\n"));
        if let Some(to) = &to {
            doc.push_str(&format!("to: {to}\n"));
        }
        doc.push_str(&format!("created_ms: {created_ms}\n"));
        doc.push_str(&format!("ttl_s: {ttl_s}\n"));
        doc.push_str("attribution: unverified-same-uid\n");
        doc.push_str("---\n");
        doc.push_str(input.body.trim());
        doc.push('\n');
        if doc.len() > super::MAX_DOC_BYTES {
            return Err(CoordinationError::WriteRefused(format!(
                "message document is {} bytes; the §9 bound is {}",
                doc.len(),
                super::MAX_DOC_BYTES
            )));
        }

        let path = writer_dir.join(format!("{id}.md"));
        let tmp = writer_dir.join(format!(".{id}.tmp"));
        {
            let mut f = std::fs::File::create(&tmp).map_err(io_err)?;
            f.write_all(doc.as_bytes()).map_err(io_err)?;
            f.sync_all().map_err(io_err)?;
        }
        super::restrict_file_modes(&tmp)?;
        std::fs::rename(&tmp, &path).map_err(io_err)?;

        Ok(MessageMeta {
            id,
            writer: writer.to_string(),
            kind: KIND_MESSAGE.to_string(),
            to,
            created_ms,
            ttl_s,
            expired: false,
        })
    }

    /// The daemon's privileged radar-note lane (§2.8, R9): writes
    /// `kind: radar-note` under the reserved `daemon` writer dir to the
    /// flagged party. The public [`Self::write`] keeps refusing the
    /// `daemon` writer — this entry point is the only path in, and its
    /// body is schema-rendered from validated tokens only (§2.3
    /// grammars; no free text): an `## overlap` section of
    /// grammar-valid repo-relative paths plus a fixed template
    /// paragraph whose only variable tokens are validated ids, the
    /// closed `sources` enum, and decimal counts.
    ///
    /// Spam discipline, both layers tested:
    /// - one live note per distinct overlap set per flagged pair — the
    ///   note id is `rn-<hash>` over (recipient, parties, pr, full
    ///   path set), so an existing file (even expired, until GC) means
    ///   this exact situation was already delivered → `Ok(None)`;
    /// - at most one note per recipient per
    ///   [`RADAR_NOTE_COOLDOWN_MS`] regardless of set — a churning
    ///   working set must not mint a note stream → `Ok(None)`.
    ///
    /// TTL takes the normal clamp (default when `None`); the per-writer
    /// live cap applies to the daemon dir like any other writer.
    pub(crate) fn write_radar_note(
        &self,
        input: &RadarNoteInput<'_>,
    ) -> Result<Option<MessageMeta>, CoordinationError> {
        let to = input.to;
        if to.is_empty() || sanitize_key(to) != to {
            return Err(CoordinationError::WriteRefused(format!(
                "radar-note recipient {to:?} is outside the filename grammar"
            )));
        }
        if input.parties.is_empty() || input.parties.len() > 8 {
            return Err(CoordinationError::WriteRefused(format!(
                "radar-note parties count {} is outside 1..=8",
                input.parties.len()
            )));
        }
        for p in input.parties {
            if p.is_empty() || sanitize_key(p) != *p {
                return Err(CoordinationError::WriteRefused(format!(
                    "radar-note party {p:?} is outside the filename grammar"
                )));
            }
        }
        if !input.parties.contains(&to) {
            return Err(CoordinationError::WriteRefused(
                "radar-note recipient must be one of its parties".into(),
            ));
        }
        if !input.declared && !input.git && input.pr.is_none() {
            return Err(CoordinationError::WriteRefused(
                "radar-note needs at least one source (declared/git/pr)".into(),
            ));
        }
        if input.paths.is_empty() {
            return Err(CoordinationError::WriteRefused(
                "radar-note needs at least one overlapping path".into(),
            ));
        }
        for p in input.paths {
            if !scan::valid_rel_path(p) {
                return Err(CoordinationError::WriteRefused(format!(
                    "radar-note path {p:?} is outside the repo-relative grammar"
                )));
            }
        }
        let ttl_s = input
            .ttl_s
            .unwrap_or(MESSAGE_TTL_DEFAULT_S)
            .clamp(MESSAGE_TTL_MIN_S, MESSAGE_TTL_MAX_S);

        // Canonical identity of this overlap situation → the note id.
        // Sorted, deduplicated paths make the hash order-independent;
        // sources are deliberately excluded so an evidence shift alone
        // (declared → declared+git) does not mint a new note.
        let mut paths: Vec<&str> = input.paths.iter().map(String::as_str).collect();
        paths.sort_unstable();
        paths.dedup();
        let mut parties: Vec<&str> = input.parties.to_vec();
        parties.sort_unstable();
        let canonical = format!(
            "to={to};parties={};pr={};paths={}",
            parties.join(","),
            input.pr.map(|n| n.to_string()).unwrap_or_default(),
            paths.join(",")
        );
        let id = format!("rn-{:016x}", super::fnv1a_64(canonical.as_bytes()));

        let writer_dir = self.dir.join(RESERVED_DAEMON_WRITER);
        if writer_dir.join(format!("{id}.md")).exists() {
            return Ok(None); // this exact overlap set was already delivered
        }
        let now_ms = super::now_ms();
        if !writer_dir.is_dir() {
            let (dirs, _) = writer_dirs(&self.dir)?;
            if dirs.len() >= MAX_WRITER_DIRS {
                return Err(CoordinationError::WriteRefused(format!(
                    "space holds {} writer dirs; the bound is {MAX_WRITER_DIRS}",
                    dirs.len()
                )));
            }
        }
        std::fs::create_dir_all(&writer_dir).map_err(io_err)?;
        restrict_dir_modes(&writer_dir)?;
        let (live, _) = scan::scan_liveness_dir(&writer_dir)?;
        if live.len() >= MAX_MESSAGES_PER_WRITER {
            return Err(CoordinationError::WriteRefused(format!(
                "writer {RESERVED_DAEMON_WRITER:?} holds {} messages; the bound is \
                 {MAX_MESSAGES_PER_WRITER} — expiry GC must catch up first",
                live.len()
            )));
        }
        // Recipient cooldown over the daemon dir's live notes.
        for entry in &live {
            let meta = match scan::open_liveness(&entry.path)? {
                DefensiveRead::Ok(bytes) => scan::parse_doc(bytes, &entry.stem, MESSAGE_KINDS)
                    .ok()
                    .and_then(|doc| {
                        meta_from_doc(&doc, &entry.stem, RESERVED_DAEMON_WRITER, now_ms)
                    }),
                _ => None,
            };
            let Some(meta) = meta else { continue };
            if meta.kind == KIND_RADAR_NOTE
                && !meta.expired
                && meta.to.as_deref() == Some(to)
                && now_ms.saturating_sub(meta.created_ms) < RADAR_NOTE_COOLDOWN_MS
            {
                return Ok(None); // recipient was noted moments ago — don't spam
            }
        }

        let mut sources: Vec<&str> = Vec::new();
        if input.declared {
            sources.push("declared");
        }
        if input.git {
            sources.push("git");
        }
        if input.pr.is_some() {
            sources.push("pr");
        }
        let sources = sources.join(",");

        let mut doc = String::new();
        doc.push_str("---\n");
        doc.push_str("v: 1\n");
        doc.push_str(&format!("kind: {KIND_RADAR_NOTE}\n"));
        doc.push_str(&format!("id: {id}\n"));
        doc.push_str(&format!("space: {}\n", self.space));
        doc.push_str(&format!("from: {RESERVED_DAEMON_WRITER}\n"));
        doc.push_str(&format!("to: {to}\n"));
        doc.push_str(&format!("parties: {}\n", parties.join(",")));
        doc.push_str(&format!("sources: {sources}\n"));
        if let Some(pr) = input.pr {
            doc.push_str(&format!("pr: {pr}\n"));
        }
        doc.push_str(&format!("created_ms: {now_ms}\n"));
        doc.push_str(&format!("ttl_s: {ttl_s}\n"));
        doc.push_str("attribution: unverified-same-uid\n");
        doc.push_str("---\n");
        doc.push_str("## overlap\n");
        let listed = paths.len().min(RADAR_NOTE_MAX_PATHS);
        for p in &paths[..listed] {
            doc.push_str(p);
            doc.push('\n');
        }
        doc.push('\n');
        let mut who = parties.join(" and ");
        if let Some(pr) = input.pr {
            who.push_str(&format!(" and open pr#{pr}"));
        }
        doc.push_str(&format!(
            "Working-set overlap between {who} (sources: {sources}). \
             Coordinate in this space before touching the paths above."
        ));
        if paths.len() > listed {
            doc.push_str(&format!(
                " {} further paths were not listed.",
                paths.len() - listed
            ));
        }
        doc.push('\n');
        if doc.len() > super::MAX_DOC_BYTES {
            return Err(CoordinationError::WriteRefused(format!(
                "radar-note document is {} bytes; the §9 bound is {}",
                doc.len(),
                super::MAX_DOC_BYTES
            )));
        }

        let path = writer_dir.join(format!("{id}.md"));
        let tmp = writer_dir.join(format!(".{id}.tmp"));
        {
            let mut f = std::fs::File::create(&tmp).map_err(io_err)?;
            f.write_all(doc.as_bytes()).map_err(io_err)?;
            f.sync_all().map_err(io_err)?;
        }
        super::restrict_file_modes(&tmp)?;
        std::fs::rename(&tmp, &path).map_err(io_err)?;

        Ok(Some(MessageMeta {
            id,
            writer: RESERVED_DAEMON_WRITER.to_string(),
            kind: KIND_RADAR_NOTE.to_string(),
            to: Some(to.to_string()),
            created_ms: now_ms,
            ttl_s,
            expired: false,
        }))
    }

    /// Every message's frontmatter across all writers, rule-5 liveness
    /// posture. Expired entries are returned flagged — GC removes,
    /// radar decides.
    pub(crate) fn scan_meta(
        &self,
        now_ms: u64,
    ) -> Result<LivenessScan<MessageMeta>, CoordinationError> {
        scan_meta_dir(&self.dir, now_ms)
    }

    /// Full read of one message.
    pub(crate) fn read(
        &self,
        writer: &str,
        id: &str,
        now_ms: u64,
    ) -> Result<Option<Message>, CoordinationError> {
        if sanitize_key(writer) != writer || sanitize_key(id) != id {
            return Ok(None);
        }
        let path = self.dir.join(writer).join(format!("{id}.md"));
        let bytes = match scan::open_liveness(&path)? {
            DefensiveRead::Ok(bytes) => bytes,
            DefensiveRead::Vanished | DefensiveRead::Reject(_) => return Ok(None),
        };
        let Ok(doc) = scan::parse_doc(bytes, id, MESSAGE_KINDS) else {
            return Ok(None);
        };
        let Some(meta) = meta_from_doc(&doc, id, writer, now_ms) else {
            return Ok(None);
        };
        Ok(Some(Message {
            meta,
            body: doc.body.clone(),
        }))
    }

    /// A writer deletes its own message (read receipts / retraction).
    pub(crate) fn delete_own(&self, writer: &str, id: &str) -> Result<bool, CoordinationError> {
        if sanitize_key(writer) != writer || sanitize_key(id) != id {
            return Err(CoordinationError::WriteRefused(
                "writer/id outside the filename grammar".into(),
            ));
        }
        match std::fs::remove_file(self.dir.join(writer).join(format!("{id}.md"))) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io_err(e)),
        }
    }
}

/// One accepted writer dir (the `ScanEntry` convention).
pub(crate) struct WriterDir {
    pub name: String,
    pub path: PathBuf,
}

/// Writer-dir enumeration: grammar-valid dirs accepted, dot entries
/// skipped, everything else a named rejection; the dir bound is hard.
pub(crate) fn writer_dirs(
    dir: &Path,
) -> Result<(Vec<WriterDir>, Vec<ScanReject>), CoordinationError> {
    let mut dirs = Vec::new();
    let mut rejected = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((dirs, rejected)),
        Err(e) => return Err(io_err(e)),
    };
    for (n, entry) in entries.enumerate() {
        if n >= MAX_WRITER_DIRS * 2 {
            return Err(CoordinationError::ReadRefused(format!(
                "{}: exceeds the writer-dir scan bound",
                dir.display()
            )));
        }
        let entry = entry.map_err(io_err)?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let meta = match std::fs::symlink_metadata(entry.path()) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err(e)),
        };
        if !meta.is_dir() || sanitize_key(&name) != name || !scan::owned_by_current_user(&meta) {
            rejected.push(ScanReject {
                name,
                reason: REJECT_FOREIGN_ENTRY,
            });
            continue;
        }
        dirs.push(WriterDir {
            name,
            path: entry.path(),
        });
    }
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((dirs, rejected))
}

/// Directory-level meta scan, shared with GC (no dir creation).
pub(crate) fn scan_meta_dir(
    dir: &Path,
    now_ms: u64,
) -> Result<LivenessScan<MessageMeta>, CoordinationError> {
    scan_meta_dir_budgeted(dir, now_ms, &mut ReadBudget::unbounded())
}

/// [`scan_meta_dir`] under a §1.6 whole-space read budget (the radar's
/// entry — one budget spans the declarations and messages of a pass):
/// budget-refused entries surface as `read-budget` rejections, unread.
pub(crate) fn scan_meta_dir_budgeted(
    dir: &Path,
    now_ms: u64,
    budget: &mut ReadBudget,
) -> Result<LivenessScan<MessageMeta>, CoordinationError> {
    let (dirs, mut rejected) = writer_dirs(dir)?;
    let mut entries = Vec::new();
    for WriterDir {
        name: writer,
        path: writer_path,
    } in dirs
    {
        let (found, sub_rejects) = scan::scan_liveness_dir(&writer_path)?;
        rejected.extend(sub_rejects.into_iter().map(|mut r| {
            r.name = format!("{writer}/{}", r.name);
            r
        }));
        for entry in found {
            let name = format!("{writer}/{}.md", entry.stem);
            if !budget.admit(entry.meta.len()) {
                rejected.push(ScanReject {
                    name,
                    reason: REJECT_READ_BUDGET,
                });
                continue;
            }
            let bytes = match scan::open_liveness(&entry.path)? {
                DefensiveRead::Ok(bytes) => bytes,
                DefensiveRead::Vanished => continue,
                DefensiveRead::Reject(reason) => {
                    rejected.push(ScanReject { name, reason });
                    continue;
                }
            };
            match scan::parse_doc(bytes, &entry.stem, MESSAGE_KINDS) {
                Ok(doc) => match meta_from_doc(&doc, &entry.stem, &writer, now_ms) {
                    Some(meta) => entries.push(meta),
                    None => rejected.push(ScanReject {
                        name,
                        reason: scan::REJECT_GRAMMAR,
                    }),
                },
                Err(reason) => rejected.push(ScanReject { name, reason }),
            }
        }
    }
    Ok(LivenessScan { entries, rejected })
}

fn meta_from_doc(doc: &scan::RawDoc, id: &str, writer: &str, now_ms: u64) -> Option<MessageMeta> {
    let created_ms = doc.field("created_ms")?.parse::<u64>().ok()?.min(now_ms);
    let ttl_s = doc
        .field("ttl_s")?
        .parse::<u32>()
        .ok()?
        .clamp(MESSAGE_TTL_MIN_S, MESSAGE_TTL_MAX_S);
    // `from` must agree with the writer dir — a note filed under
    // another writer's dir claims an attribution it doesn't have.
    if doc.field("from") != Some(writer) {
        return None;
    }
    let to = doc.field("to").map(str::to_string);
    if let Some(t) = &to {
        if sanitize_key(t) != *t {
            return None;
        }
    }
    Some(MessageMeta {
        id: id.to_string(),
        writer: writer.to_string(),
        kind: doc.field("kind").unwrap_or(KIND_MESSAGE).to_string(),
        to,
        created_ms,
        ttl_s,
        expired: now_ms.saturating_sub(created_ms) > u64::from(ttl_s) * 1000,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(tmp: &tempfile::TempDir) -> MessageSpace {
        MessageSpace::open(&tmp.path().join("space"), "test-space").unwrap()
    }

    fn msg(body: &str) -> MessageInput<'_> {
        MessageInput {
            to: None,
            ttl_s: None,
            body,
        }
    }

    #[test]
    fn write_scan_read_delete_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let ms = space(&tmp);
        let m = ms
            .write(
                "s-alpha",
                &MessageInput {
                    to: Some("s-beta"),
                    ttl_s: Some(3600),
                    body: "heads up: touching tools.rs next",
                },
            )
            .unwrap();
        assert_eq!(m.ttl_s, 3600);
        assert!(!m.expired);

        let now = super::super::now_ms();
        let scan = ms.scan_meta(now).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert!(scan.rejected.is_empty());
        assert_eq!(scan.entries[0].to.as_deref(), Some("s-beta"));

        let full = ms.read("s-alpha", &m.id, now).unwrap().unwrap();
        assert_eq!(full.body, "heads up: touching tools.rs next");

        assert!(ms.delete_own("s-alpha", &m.id).unwrap());
        assert!(!ms.delete_own("s-alpha", &m.id).unwrap());
        assert!(ms.scan_meta(now).unwrap().entries.is_empty());
    }

    #[test]
    fn ttl_clamps_and_expiry_flags() {
        let tmp = tempfile::tempdir().unwrap();
        let ms = space(&tmp);
        let m = ms
            .write(
                "s-a",
                &MessageInput {
                    to: None,
                    ttl_s: Some(1),
                    body: "x",
                },
            )
            .unwrap();
        assert_eq!(m.ttl_s, MESSAGE_TTL_MIN_S, "hint clamped, not refused");
        let m2 = ms
            .write(
                "s-a",
                &MessageInput {
                    to: None,
                    ttl_s: Some(u32::MAX),
                    body: "y",
                },
            )
            .unwrap();
        assert_eq!(m2.ttl_s, MESSAGE_TTL_MAX_S);

        let later = super::super::now_ms() + u64::from(MESSAGE_TTL_MIN_S) * 1000 + 1500;
        let scan = ms.scan_meta(later).unwrap();
        let short = scan.entries.iter().find(|e| e.id == m.id).unwrap();
        assert!(short.expired);
        let long = scan.entries.iter().find(|e| e.id == m2.id).unwrap();
        assert!(!long.expired);
    }

    #[test]
    fn refusals_are_named() {
        let tmp = tempfile::tempdir().unwrap();
        let ms = space(&tmp);
        assert!(ms
            .write("Bad Writer", &msg("x"))
            .unwrap_err()
            .to_string()
            .contains("grammar"));
        assert!(ms
            .write("daemon", &msg("x"))
            .unwrap_err()
            .to_string()
            .contains("reserved"));
        assert!(ms
            .write("s-a", &msg("   "))
            .unwrap_err()
            .to_string()
            .contains("non-empty"));
        let mut bad_to = msg("x");
        bad_to.to = Some("No Good");
        assert!(ms
            .write("s-a", &bad_to)
            .unwrap_err()
            .to_string()
            .contains("grammar"));
    }

    #[test]
    fn per_writer_cap_refuses_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let ms = space(&tmp);
        for _ in 0..MAX_MESSAGES_PER_WRITER {
            ms.write("s-chatty", &msg("spam")).unwrap();
        }
        let err = ms.write("s-chatty", &msg("one more")).unwrap_err();
        assert!(err.to_string().contains("bound"), "{err}");
        // A different writer is unaffected.
        ms.write("s-quiet", &msg("fine")).unwrap();
    }

    #[test]
    fn misattributed_from_field_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let ms = space(&tmp);
        let m = ms.write("s-real", &msg("legit")).unwrap();
        // Copy the doc under another writer's dir: same bytes, wrong dir.
        let src = tmp
            .path()
            .join("space/messages/s-real")
            .join(format!("{}.md", m.id));
        let forged_dir = tmp.path().join("space/messages/s-forger");
        std::fs::create_dir_all(&forged_dir).unwrap();
        std::fs::copy(&src, forged_dir.join(format!("{}.md", m.id))).unwrap();

        let scan = ms.scan_meta(super::super::now_ms()).unwrap();
        assert_eq!(scan.entries.len(), 1, "forged copy rejected");
        assert_eq!(scan.rejected.len(), 1);
        assert!(scan.rejected[0].name.starts_with("s-forger/"));
    }

    fn note<'a>(to: &'a str, parties: &'a [&'a str], paths: &'a [String]) -> RadarNoteInput<'a> {
        RadarNoteInput {
            to,
            parties,
            declared: true,
            git: true,
            pr: None,
            paths,
            ttl_s: None,
        }
    }

    #[test]
    fn radar_note_writes_under_daemon_and_reads_back() {
        let tmp = tempfile::tempdir().unwrap();
        let ms = space(&tmp);
        let paths = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let meta = ms
            .write_radar_note(&note("s-alpha", &["s-alpha", "s-beta"], &paths))
            .unwrap()
            .expect("first note lands");
        assert_eq!(meta.writer, RESERVED_DAEMON_WRITER);
        assert_eq!(meta.kind, KIND_RADAR_NOTE);
        assert_eq!(meta.to.as_deref(), Some("s-alpha"));
        assert_eq!(meta.ttl_s, MESSAGE_TTL_DEFAULT_S, "normal TTL clamp");
        assert!(meta.id.starts_with("rn-"), "{}", meta.id);
        assert_eq!(sanitize_key(&meta.id), meta.id, "id obeys the grammar");

        // The scan lists it like any message; the full read shows the
        // schema-rendered body: `## overlap` paths + template line.
        let now = super::super::now_ms();
        let scan = ms.scan_meta(now).unwrap();
        assert!(scan.rejected.is_empty(), "{:?}", scan.rejected);
        assert_eq!(scan.entries.len(), 1);
        let full = ms
            .read(RESERVED_DAEMON_WRITER, &meta.id, now)
            .unwrap()
            .unwrap();
        assert_eq!(
            full.body,
            "## overlap\nsrc/a.rs\nsrc/b.rs\n\nWorking-set overlap between s-alpha and s-beta \
             (sources: declared,git). Coordinate in this space before touching the paths above."
        );
        let raw = std::fs::read_to_string(
            tmp.path()
                .join("space/messages/daemon")
                .join(format!("{}.md", meta.id)),
        )
        .unwrap();
        assert!(raw.contains("from: daemon\n"), "{raw}");
        assert!(raw.contains("parties: s-alpha,s-beta\n"), "{raw}");
        assert!(raw.contains("sources: declared,git\n"), "{raw}");
        assert!(raw.contains("attribution: unverified-same-uid\n"));
    }

    #[test]
    fn radar_note_pr_variant_carries_pr_and_source() {
        let tmp = tempfile::tempdir().unwrap();
        let ms = space(&tmp);
        let paths = vec!["docs/x.md".to_string()];
        let meta = ms
            .write_radar_note(&RadarNoteInput {
                to: "s-alpha",
                parties: &["s-alpha"],
                declared: false,
                git: true,
                pr: Some(566),
                paths: &paths,
                ttl_s: Some(3600),
            })
            .unwrap()
            .unwrap();
        assert_eq!(meta.ttl_s, 3600);
        let now = super::super::now_ms();
        let full = ms
            .read(RESERVED_DAEMON_WRITER, &meta.id, now)
            .unwrap()
            .unwrap();
        assert!(
            full.body.contains("s-alpha and open pr#566"),
            "{}",
            full.body
        );
        assert!(full.body.contains("(sources: git,pr)"), "{}", full.body);
        let raw = std::fs::read_to_string(
            tmp.path()
                .join("space/messages/daemon")
                .join(format!("{}.md", meta.id)),
        )
        .unwrap();
        assert!(raw.contains("pr: 566\n"), "{raw}");
    }

    #[test]
    fn radar_note_dedups_by_overlap_set_and_cools_down_per_recipient() {
        let tmp = tempfile::tempdir().unwrap();
        let ms = space(&tmp);
        let paths = vec!["src/a.rs".to_string()];
        let parties = ["s-alpha", "s-beta"];
        let first = ms
            .write_radar_note(&note("s-alpha", &parties, &paths))
            .unwrap();
        assert!(first.is_some());
        // Same set again (paths in any order): deduped by id.
        let again = ms
            .write_radar_note(&note("s-alpha", &parties, &paths))
            .unwrap();
        assert!(again.is_none(), "identical overlap set never re-notes");
        // A different set for the same recipient: suppressed by the
        // 10-minute recipient cooldown, not written.
        let other_paths = vec!["src/z.rs".to_string()];
        let cooled = ms
            .write_radar_note(&note("s-alpha", &parties, &other_paths))
            .unwrap();
        assert!(cooled.is_none(), "recipient cooldown holds");
        // The OTHER flagged party is a distinct recipient: lands.
        let beta = ms
            .write_radar_note(&note("s-beta", &parties, &paths))
            .unwrap();
        assert!(beta.is_some(), "each flagged party gets its own note");
        let scan = ms.scan_meta(super::super::now_ms()).unwrap();
        assert_eq!(scan.entries.len(), 2, "one per recipient, no spam");
    }

    #[test]
    fn radar_note_refusals_are_named() {
        let tmp = tempfile::tempdir().unwrap();
        let ms = space(&tmp);
        let good = vec!["src/a.rs".to_string()];
        let hostile = vec!["../etc/passwd".to_string()];
        for (input, needle) in [
            (note("Bad Id", &["Bad Id"], &good), "grammar"),
            (note("s-a", &["s-b"], &good), "one of its parties"),
            (note("s-a", &[], &good), "1..=8"),
            (note("s-a", &["s-a"], &hostile), "repo-relative"),
            (
                RadarNoteInput {
                    to: "s-a",
                    parties: &["s-a"],
                    declared: false,
                    git: false,
                    pr: None,
                    paths: &good,
                    ttl_s: None,
                },
                "at least one source",
            ),
            (
                RadarNoteInput {
                    to: "s-a",
                    parties: &["s-a"],
                    declared: true,
                    git: false,
                    pr: None,
                    paths: &[],
                    ttl_s: None,
                },
                "at least one overlapping path",
            ),
        ] {
            let err = ms.write_radar_note(&input).unwrap_err().to_string();
            assert!(err.contains(needle), "{err}");
        }
    }

    #[test]
    fn budgeted_meta_scan_skips_unread_messages_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let ms = space(&tmp);
        for body in ["one", "two", "three"] {
            ms.write("s-a", &msg(body)).unwrap();
        }
        let dir = tmp.path().join("space/messages");
        let mut budget = ReadBudget::new(1, u64::MAX);
        let scan = scan_meta_dir_budgeted(&dir, super::super::now_ms(), &mut budget).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.rejected.len(), 2);
        assert!(scan
            .rejected
            .iter()
            .all(|r| r.reason == REJECT_READ_BUDGET && r.name.starts_with("s-a/")));
    }

    #[test]
    fn foreign_entries_in_messages_root_surface_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let ms = space(&tmp);
        ms.write("s-a", &msg("fine")).unwrap();
        std::fs::write(tmp.path().join("space/messages/stray-file"), "x").unwrap();
        let scan = ms.scan_meta(super::super::now_ms()).unwrap();
        assert_eq!(scan.entries.len(), 1);
        assert_eq!(scan.rejected.len(), 1);
        assert_eq!(scan.rejected[0].reason, REJECT_FOREIGN_ENTRY);
    }
}
