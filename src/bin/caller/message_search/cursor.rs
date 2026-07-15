//! Per-source-file read cursors (message-search plan §6): enough identity
//! to distinguish "bytes were appended" (incremental read past the saved
//! offset) from "the file was rewritten or replaced" (rebuild that
//! session's shard). `FileIdentity` alone is degenerate on some Windows
//! filesystems, so the cursor folds len + timestamps exactly like the
//! catalog's cache keys, plus content hashes that catch in-place prefix
//! rewrites identity+len+mtime can miss (mtime granularity,
//! mtime-preserving writers).

use crate::platform::FileIdentity;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Bytes hashed from the file head for the prefix fingerprint. Shared
/// with the session catalog's incremental row accumulators, which use the
/// same head-hash to distinguish appends from rewrites.
pub(crate) const PREFIX_HASH_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SourceCursor {
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<FileIdentity>,
    pub len: u64,
    pub mtime_ms: i64,
    /// Offset just past the last COMPLETE line consumed — a partial
    /// trailing line (no newline yet) stays unread until it completes.
    pub last_complete_line_offset: u64,
    pub prefix_hash16: String,
    /// Second rewrite-detection window: hash of the last
    /// `min(4096, last_complete_line_offset)` bytes ENDING at the consumed
    /// offset. The head window alone cannot exclude a rewrite past the
    /// first 4 KiB that also grows the file (it would read as a benign
    /// append); requiring both windows narrows the undetected residual to
    /// a rewrite that preserves the head, the pre-consumed tail, AND grows
    /// the file — accepted and documented. Empty on cursors persisted
    /// before the field existed: those classify as before but are never
    /// eligible for incremental resume.
    #[serde(default)]
    pub consumed_tail_hash16: String,
    pub parser_version: u32,
}

/// What a fresh look at the file says relative to a saved cursor.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CursorCheck {
    /// Nothing new past the cursor.
    Unchanged,
    /// Bytes were appended; read from `last_complete_line_offset`.
    Appended,
    /// Identity/len/prefix changed (replacement, truncation, in-place
    /// rewrite) or the parser version moved: rebuild from scratch.
    Rewritten,
    /// The file is gone (source GC / lease cleanup); keep the shard until
    /// window expiry with `source_gone` coverage (plan §6).
    Gone,
}

fn file_mtime_ms(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn prefix_hash16(path: &Path) -> Option<String> {
    prefix_hash16_bytes(path, PREFIX_HASH_BYTES)
}

/// Hash16 over the first `max_bytes` of `path` (fewer at EOF).
pub(crate) fn prefix_hash16_bytes(path: &Path, max_bytes: usize) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; max_bytes];
    let mut read = 0usize;
    loop {
        if read == buf.len() {
            break;
        }
        match file.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(_) => return None,
        }
    }
    buf.truncate(read);
    Some(hash16(&buf))
}

/// Hash16 over the `min(4096, end)` bytes ENDING at offset `end` — the
/// second rewrite-detection window (see `consumed_tail_hash16`). For
/// consumed ranges under 4 KiB this window covers every consumed byte,
/// which is what closes the small-file head-mutation hole the prefix
/// window's "not comparable on growth" rule leaves open.
pub(crate) fn tail_hash16_ending_at(path: &Path, end: u64) -> Option<String> {
    let window = end.min(PREFIX_HASH_BYTES as u64);
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(end - window)).ok()?;
    let mut buf = vec![0u8; window as usize];
    let mut read = 0usize;
    loop {
        if read == buf.len() {
            break;
        }
        match file.read(&mut buf[read..]) {
            // Shorter than the recorded consumed range: a concurrent
            // truncation; the caller's length checks classify it.
            Ok(0) => return None,
            Ok(n) => read += n,
            Err(_) => return None,
        }
    }
    Some(hash16(&buf))
}

fn hash16(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

impl SourceCursor {
    /// Cursor for a file we have consumed through `last_complete_line_offset`.
    pub(crate) fn capture(path: &Path, last_complete_line_offset: u64) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            identity: FileIdentity::from_path(path).ok(),
            len: metadata.len(),
            mtime_ms: file_mtime_ms(&metadata),
            last_complete_line_offset,
            prefix_hash16: prefix_hash16(path)?,
            consumed_tail_hash16: tail_hash16_ending_at(path, last_complete_line_offset)?,
            parser_version: super::record::PARSER_VERSION,
        })
    }

    /// Whether this cursor carries the second (consumed-tail) hash window.
    /// Incremental resume REQUIRES it: cursors persisted before the field
    /// existed classify exactly as before, but never resume — the next
    /// full re-parse re-captures with both windows.
    pub(crate) fn supports_incremental_resume(&self) -> bool {
        !self.consumed_tail_hash16.is_empty()
    }

    /// Compare the file on disk against this cursor.
    pub(crate) fn check(&self) -> CursorCheck {
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            return CursorCheck::Gone;
        };
        if self.parser_version != super::record::PARSER_VERSION {
            return CursorCheck::Rewritten;
        }
        if metadata.len() < self.len {
            return CursorCheck::Rewritten;
        }
        let current_identity = FileIdentity::from_path(&self.path).ok();
        let identity_reliably_same = match (self.identity, current_identity) {
            (Some(saved), Some(current)) if saved.is_reliable() && current.is_reliable() => {
                if saved != current {
                    return CursorCheck::Rewritten;
                }
                true
            }
            _ => false,
        };
        let mtime_ms = file_mtime_ms(&metadata);
        // Same length, moved mtime: an in-place rewrite whose changed bytes
        // lie past the hashed prefix window would otherwise read as
        // Unchanged forever (the module doc's "folds len + timestamps"
        // promise). A benign touch(1) costs one idempotent rebuild.
        if metadata.len() == self.len && mtime_ms != self.mtime_ms {
            return CursorCheck::Rewritten;
        }
        // Cheap-facts short-circuit: a reliable identity match with
        // unchanged len + mtime is the steady state of nearly every
        // in-retention file on every 30s sweep — skip the open + 4 KiB
        // read + SHA-256 that used to run merely to confirm it. Any
        // rewrite these facts can miss (same-length mtime-preserving
        // writer) was equally invisible to the prefix hash past 4 KiB.
        let hash_needed =
            !(identity_reliably_same && metadata.len() == self.len && mtime_ms == self.mtime_ms);
        if hash_needed {
            match prefix_hash16(&self.path) {
                Some(hash) => {
                    // The saved and fresh hashes cover the same byte window
                    // only when the saved file already filled the window, or
                    // the length hasn't changed — an append to a small file
                    // widens the hashed window and is NOT comparable (and is
                    // exactly the benign case the offset check below handles;
                    // the consumed-tail window below covers every consumed
                    // byte of such a small file, so a head mutation on a
                    // grown small file still reads as Rewritten).
                    let comparable =
                        self.len as usize >= PREFIX_HASH_BYTES || metadata.len() == self.len;
                    if comparable && hash != self.prefix_hash16 {
                        return CursorCheck::Rewritten;
                    }
                }
                None => return CursorCheck::Gone,
            }
            // Second window: the head hash alone cannot exclude a rewrite
            // past the first 4 KiB that also grows the file — it would
            // read as a benign append. An honest append never changes
            // bytes at or before the consumed offset, so a moved tail
            // window is proof of rewrite. (Both-windows-preserved growth
            // rewrites remain the documented residual.)
            if !self.consumed_tail_hash16.is_empty() {
                match tail_hash16_ending_at(&self.path, self.last_complete_line_offset) {
                    Some(hash) if hash == self.consumed_tail_hash16 => {}
                    Some(_) => return CursorCheck::Rewritten,
                    None => return CursorCheck::Gone,
                }
            }
        }
        if metadata.len() > self.last_complete_line_offset {
            CursorCheck::Appended
        } else {
            CursorCheck::Unchanged
        }
    }
}

/// Stream complete lines from `path` starting at `offset` through
/// `consume`; returns the offset just past the last complete line (a
/// trailing partial line is left for the next pass — plan §6).
///
/// Streaming, not collecting: extraction runs on every changed
/// in-retention source every sweep, and materializing a multi-hundred-MB
/// rollout as an owned `Vec<String>` was a file-sized allocation spike
/// per parse. One reused line buffer serves the whole read.
pub(crate) fn for_each_complete_line_from(
    path: &Path,
    offset: u64,
    mut consume: impl FnMut(&str),
) -> std::io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = std::io::BufReader::new(file);
    let mut consumed = offset;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let bytes = reader.read_until(b'\n', &mut buf)?;
        if bytes == 0 {
            break;
        }
        if buf.last() != Some(&b'\n') {
            // Partial trailing line: wait for it to complete.
            break;
        }
        consumed += bytes as u64;
        let line = String::from_utf8_lossy(&buf);
        consume(line.trim_end_matches(['\n', '\r']));
    }
    Ok(consumed)
}

/// Collected form of [`for_each_complete_line_from`] — tests and small
/// bounded reads only; extraction paths must stream.
#[cfg(test)]
pub(crate) fn read_complete_lines_from(
    path: &Path,
    offset: u64,
) -> std::io::Result<(Vec<String>, u64)> {
    let mut lines = Vec::new();
    let consumed = for_each_complete_line_from(path, offset, |line| lines.push(line.to_string()))?;
    Ok((lines, consumed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn append_and_partial_line_handling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, "one\ntwo\npar").unwrap();

        let (lines, consumed) = read_complete_lines_from(&path, 0).unwrap();
        assert_eq!(lines, vec!["one", "two"]);
        assert_eq!(consumed, 8);

        let cursor = SourceCursor::capture(&path, consumed).unwrap();
        assert_eq!(cursor.check(), CursorCheck::Appended); // partial bytes exist

        // Completing the line + appending another becomes readable.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"tial\nthree\n").unwrap();
        drop(file);
        let (lines, consumed) = read_complete_lines_from(&path, consumed).unwrap();
        assert_eq!(lines, vec!["partial", "three"]);
        let cursor = SourceCursor::capture(&path, consumed).unwrap();
        assert_eq!(cursor.check(), CursorCheck::Unchanged);
    }

    #[test]
    fn truncation_and_replacement_read_as_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, "aaaa\nbbbb\n").unwrap();
        let cursor = SourceCursor::capture(&path, 10).unwrap();

        std::fs::write(&path, "cc\n").unwrap(); // truncated + different bytes
        assert_eq!(cursor.check(), CursorCheck::Rewritten);
    }

    #[test]
    fn same_length_prefix_rewrite_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, "aaaa\nbbbb\n").unwrap();
        let cursor = SourceCursor::capture(&path, 10).unwrap();
        std::fs::write(&path, "zzzz\nbbbb\n").unwrap(); // same len, new head
        assert_eq!(cursor.check(), CursorCheck::Rewritten);
    }

    #[test]
    fn post_prefix_rewrite_that_grows_reads_as_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        // First line fills the 4 KiB prefix window; the second is the
        // post-prefix content a growth rewrite mutates.
        let head = format!("{}\n", "h".repeat(PREFIX_HASH_BYTES));
        let body = format!("{head}old-tail-line\n");
        std::fs::write(&path, &body).unwrap();
        let cursor = SourceCursor::capture(&path, body.len() as u64).unwrap();
        assert!(cursor.supports_incremental_resume());

        // Same head 4 KiB, mutated bytes past it, file GROWN: the head
        // window matches and the length check passes — only the
        // consumed-tail window can (and must) catch it.
        std::fs::write(&path, format!("{head}new-tail-line\nappended line\n")).unwrap();
        assert_eq!(cursor.check(), CursorCheck::Rewritten);
    }

    #[test]
    fn small_file_head_mutating_grow_reads_as_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, "aaaa\nbbbb\n").unwrap(); // well under 4 KiB
        let cursor = SourceCursor::capture(&path, 10).unwrap();

        // Head mutated AND grown: the prefix window is "not comparable"
        // (saved len < window, length changed), which used to read as a
        // benign append; the consumed-tail window covers every consumed
        // byte of a small file and catches it.
        std::fs::write(&path, "zzzz\nbbbb\ncccc\n").unwrap();
        assert_eq!(cursor.check(), CursorCheck::Rewritten);
    }

    #[test]
    fn honest_append_keeps_both_windows_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let head = format!("{}\n", "h".repeat(PREFIX_HASH_BYTES));
        let body = format!("{head}tail-line\n");
        std::fs::write(&path, &body).unwrap();
        let cursor = SourceCursor::capture(&path, body.len() as u64).unwrap();

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"appended line\n").unwrap();
        drop(file);
        assert_eq!(cursor.check(), CursorCheck::Appended);
    }

    #[test]
    fn missing_file_reads_as_gone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, "x\n").unwrap();
        let cursor = SourceCursor::capture(&path, 2).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(cursor.check(), CursorCheck::Gone);
    }

    #[test]
    fn parser_version_bump_forces_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, "x\n").unwrap();
        let mut cursor = SourceCursor::capture(&path, 2).unwrap();
        cursor.parser_version = 0;
        assert_eq!(cursor.check(), CursorCheck::Rewritten);
    }
}
