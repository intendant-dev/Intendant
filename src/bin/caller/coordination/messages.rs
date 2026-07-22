//! Message kind (Track C, C1): the `messages/<writer>/` async mail
//! lane. A writer leaves bounded, TTL'd notes for the space (or a
//! named recipient); readers poll. Delivery is best-effort liveness
//! data — never an instruction channel: bodies are DATA the reader
//! weighs, and expiry is advisory until GC removes the file. The
//! `daemon` writer name is reserved for the daemon's own lanes
//! (C2 radar notes); plain writers are refused it here.
#![cfg_attr(not(test), allow(dead_code))] // C1 PR A: consumed by the C2/C3 lanes + skill; allow dropped as wiring lands.

use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use super::scan::{self, DefensiveRead, LivenessScan, ScanReject, REJECT_FOREIGN_ENTRY};
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
