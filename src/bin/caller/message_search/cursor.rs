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

/// Bytes hashed from the file head for the prefix fingerprint.
const PREFIX_HASH_BYTES: usize = 4096;

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
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; PREFIX_HASH_BYTES];
    let mut read = 0usize;
    loop {
        match file.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(_) => return None,
        }
        if read == buf.len() {
            break;
        }
    }
    buf.truncate(read);
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
            parser_version: super::record::PARSER_VERSION,
        })
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
        if let (Some(saved), Ok(current)) = (self.identity, FileIdentity::from_path(&self.path)) {
            if saved.is_reliable() && current.is_reliable() && saved != current {
                return CursorCheck::Rewritten;
            }
        }
        match prefix_hash16(&self.path) {
            Some(hash) => {
                // The saved and fresh hashes cover the same byte window
                // only when the saved file already filled the window, or
                // the length hasn't changed — an append to a small file
                // widens the hashed window and is NOT comparable (and is
                // exactly the benign case the offset check below handles).
                let comparable =
                    self.len as usize >= PREFIX_HASH_BYTES || metadata.len() == self.len;
                if comparable && hash != self.prefix_hash16 {
                    return CursorCheck::Rewritten;
                }
            }
            None => return CursorCheck::Gone,
        }
        if metadata.len() > self.last_complete_line_offset {
            CursorCheck::Appended
        } else {
            CursorCheck::Unchanged
        }
    }
}

/// Read complete lines from `path` starting at `offset`; returns the
/// lines and the offset just past the last complete line (a trailing
/// partial line is left for the next pass — plan §6).
pub(crate) fn read_complete_lines_from(
    path: &Path,
    offset: u64,
) -> std::io::Result<(Vec<String>, u64)> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = std::io::BufReader::new(file);
    let mut lines = Vec::new();
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
        let trimmed = line.trim_end_matches(['\n', '\r']);
        lines.push(trimmed.to_string());
    }
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
