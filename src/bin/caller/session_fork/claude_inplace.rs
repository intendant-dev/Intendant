//! Same-id in-place truncation of a Claude Code transcript — the
//! pencil's surgery rung. Cuts the session's OWN file at the edited
//! user row (minus its leading queue bookkeeping) so a plain
//! `--resume <same id>` continues from just before that message with the
//! edited prompt as the next turn. Unlike the fork rung's chain-slice
//! copy, the session id and directory survive, so every sid-keyed
//! sidecar (`subagents/`, todos) stays attached by construction.
//!
//! Semantics pinned by the 2026-07-27 in-place-edit probes (CC 2.1.220,
//! `tests/skills/session-fork-probes/SKILL.md`): resume walks the chain
//! back from the physical tail, torn tails are skipped, and chain
//! topology is not strictly enforced on load — physical truncation, not
//! parentUuid repair, is what guarantees the prune.
//!
//! HARD PRECONDITION (caller-enforced): no Claude Code process may be
//! attached to the session — a live CLI's close-flush appends rows and
//! would race or undo the cut. The supervisor stops the wrapper and
//! waits for transcript quiescence before calling; as a backstop the
//! execute step re-stats the file and aborts if it moved under us.

use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct ClaudeInplaceOutcome {
    pub(crate) kept_lines: usize,
    pub(crate) backup_path: PathBuf,
}

/// Row types that are queue bookkeeping for the user row that follows
/// them: a cut at a user row also drops its contiguous run of these.
fn is_queue_bookkeeping(row_type: Option<&str>) -> bool {
    matches!(row_type, Some("queue-operation") | Some("queued_command"))
}

/// Truncate `transcript` in place just before `target_user_uuid` (and
/// its leading queue-bookkeeping run). One read decides everything;
/// the kept prefix is the file's own verbatim bytes (no
/// re-serialization). Atomic: tmp + fsync + rename, with the original
/// preserved beside the transcript as `.bak-edit-<ms>`.
///
/// Refusals (`Err`) leave the transcript untouched:
/// - target missing, or not a `user` row, or off the active chain;
/// - a first-message cut (nothing but meta would survive — resume
///   behavior on a message-less transcript is unproven, and the fork
///   rung refuses the same case);
/// - the file changed between read and rename (attached-process race).
pub(crate) fn truncate_claude_transcript_in_place(
    transcript: &Path,
    target_user_uuid: &str,
) -> Result<ClaudeInplaceOutcome, String> {
    let stat_before = std::fs::metadata(transcript)
        .map_err(|err| format!("failed to stat the transcript: {err}"))?;
    let raw = std::fs::read_to_string(transcript)
        .map_err(|err| format!("failed to read the transcript: {err}"))?;

    // Line starts by byte offset, so the kept prefix is byte-verbatim.
    let mut line_starts: Vec<usize> = vec![0];
    for (index, byte) in raw.bytes().enumerate() {
        if byte == b'\n' && index + 1 < raw.len() {
            line_starts.push(index + 1);
        }
    }
    let lines: Vec<&str> = raw.lines().collect();
    let mut target_line: Option<usize> = None;
    let mut parsed_types: Vec<Option<String>> = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        let value = serde_json::from_str::<serde_json::Value>(line.trim()).ok();
        let row_type = value
            .as_ref()
            .and_then(|v| v.get("type"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        if target_line.is_none()
            && value
                .as_ref()
                .and_then(|v| v.get("uuid"))
                .and_then(serde_json::Value::as_str)
                == Some(target_user_uuid)
        {
            if row_type.as_deref() != Some("user") {
                return Err(format!(
                    "message {target_user_uuid} is not a user row — refusing to cut there"
                ));
            }
            target_line = Some(index);
        }
        parsed_types.push(row_type);
    }
    let Some(target_line) = target_line else {
        return Err(format!(
            "message {target_user_uuid} not found in the transcript \
             (history may have moved since the edit was resolved)"
        ));
    };
    let tree = super::parse_claude_transcript_tree_from_lines(lines.iter().copied());
    let Some(leaf) = tree.active_leaf.as_deref() else {
        return Err("the transcript has no active message chain".to_string());
    };
    if !tree
        .ancestor_chain(leaf)
        .iter()
        .any(|node| node.uuid == target_user_uuid)
    {
        return Err(format!(
            "message {target_user_uuid} is not on the session's active chain — \
             it may sit on an abandoned branch"
        ));
    }

    // The cut also drops the target row's leading queue bookkeeping.
    let mut cut_line = target_line;
    while cut_line > 0 && is_queue_bookkeeping(parsed_types[cut_line - 1].as_deref()) {
        cut_line -= 1;
    }
    // At least one real message row must survive the cut: a meta-only
    // transcript's resume behavior is unproven, and losing the whole
    // conversation is never what a pencil edit means.
    let keeps_a_message_row = (0..cut_line).any(|index| {
        matches!(
            parsed_types[index].as_deref(),
            Some("user") | Some("assistant")
        )
    });
    if !keeps_a_message_row {
        return Err("cannot rewind in place before the session's first message".to_string());
    }

    let cut_offset = line_starts
        .get(cut_line)
        .copied()
        .ok_or_else(|| "cut line has no byte offset".to_string())?;
    let kept = &raw.as_bytes()[..cut_offset];

    let parent_dir = transcript
        .parent()
        .ok_or_else(|| "transcript has no parent directory".to_string())?;
    let file_name = transcript
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "transcript has no file name".to_string())?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let backup_path = parent_dir.join(format!("{file_name}.bak-edit-{now_ms}"));
    let tmp_path = parent_dir.join(format!(".{file_name}.tmp-edit-{now_ms}"));

    let cleanup = |paths: &[&Path]| {
        for path in paths {
            let _ = std::fs::remove_file(path);
        }
    };

    // Backup first (the bytes we read, not a racy re-read), then the
    // kept prefix through tmp + fsync + atomic rename.
    std::fs::write(&backup_path, raw.as_bytes())
        .map_err(|err| format!("failed to write the pre-edit backup: {err}"))?;
    let write_tmp = || -> std::io::Result<()> {
        let mut tmp = std::fs::File::create(&tmp_path)?;
        tmp.write_all(kept)?;
        tmp.sync_all()
    };
    if let Err(err) = write_tmp() {
        cleanup(&[&backup_path, &tmp_path]);
        return Err(format!("failed to stage the truncated transcript: {err}"));
    }

    // Attached-process backstop: if the file moved since our read, the
    // rename would clobber rows we never saw. Abort instead.
    match std::fs::metadata(transcript) {
        Ok(stat_now)
            if stat_now.len() == stat_before.len()
                && stat_now.modified().ok() == stat_before.modified().ok() => {}
        Ok(_) => {
            cleanup(&[&backup_path, &tmp_path]);
            return Err(
                "the transcript changed during surgery (a process is writing it) — aborted"
                    .to_string(),
            );
        }
        Err(err) => {
            cleanup(&[&backup_path, &tmp_path]);
            return Err(format!("failed to re-stat the transcript: {err}"));
        }
    }
    if let Err(err) = std::fs::rename(&tmp_path, transcript) {
        cleanup(&[&backup_path, &tmp_path]);
        return Err(format!("failed to swap in the truncated transcript: {err}"));
    }
    // Durability of the rename itself, where the platform allows a
    // directory handle sync (no-op elsewhere).
    #[cfg(unix)]
    {
        let _ = std::fs::File::open(parent_dir).and_then(|dir| dir.sync_all());
    }

    Ok(ClaudeInplaceOutcome {
        kept_lines: cut_line,
        backup_path,
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_fixtures::message_line;
    use super::*;
    use sha2::Digest;

    fn queue_row(operation: &str) -> String {
        serde_json::json!({
            "type": "queue-operation",
            "operation": operation,
            "timestamp": "2026-07-27T00:00:00.000Z",
            "sessionId": "aaaaaaaa-0000-0000-0000-000000000000",
        })
        .to_string()
    }

    fn write_transcript(dir: &Path, lines: &[String]) -> PathBuf {
        let path = dir.join("aaaaaaaa-0000-0000-0000-000000000000.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").expect("write transcript");
        path
    }

    fn sha256(path: &Path) -> String {
        format!(
            "{:x}",
            sha2::Sha256::digest(std::fs::read(path).expect("read"))
        )
    }

    #[test]
    fn inplace_truncation_cuts_queue_operation_prefix_and_is_atomic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lines = [
            message_line("u1", None, "user", "first prompt", false),
            message_line("a1", Some("u1"), "assistant", "reply one", false),
            queue_row("enqueue"),
            queue_row("dequeue"),
            message_line("u2", Some("a1"), "user", "edit me", false),
            message_line("a2", Some("u2"), "assistant", "stale answer", false),
        ];
        let path = write_transcript(dir.path(), &lines);
        let original = std::fs::read_to_string(&path).expect("original");

        let outcome = truncate_claude_transcript_in_place(&path, "u2").expect("truncate");
        assert_eq!(outcome.kept_lines, 2, "queue rows must fall with the cut");

        let truncated = std::fs::read_to_string(&path).expect("truncated");
        assert!(truncated.contains("reply one"));
        assert!(!truncated.contains("edit me"));
        assert!(!truncated.contains("queue-operation"));
        assert!(!truncated.contains("stale answer"));
        // Byte-verbatim prefix of the original — no re-serialization.
        assert!(original.starts_with(&truncated));

        // The backup carries the FULL original; no tmp strays remain.
        assert_eq!(
            std::fs::read_to_string(&outcome.backup_path).expect("backup"),
            original
        );
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-edit-"))
            .collect();
        assert!(strays.is_empty(), "tmp strays: {strays:?}");
    }

    /// The commission's pin: sid-keyed sidecars (the fork lane's known
    /// blindspot) survive the surgery byte-identical, because the id and
    /// directory are never touched.
    #[test]
    fn surgery_preserves_subagent_sidecars_byte_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lines = [
            message_line("u1", None, "user", "spawn the task", false),
            message_line("a1", Some("u1"), "assistant", "spawned", false),
            message_line("u2", Some("a1"), "user", "edit me", false),
            message_line("a2", Some("u2"), "assistant", "stale", false),
        ];
        let path = write_transcript(dir.path(), &lines);

        let subagents = dir
            .path()
            .join("aaaaaaaa-0000-0000-0000-000000000000")
            .join("subagents");
        std::fs::create_dir_all(&subagents).expect("subagents dir");
        let sidecar = subagents.join("agent-42cafe.jsonl");
        let sidecar_meta = subagents.join("agent-42cafe.meta.json");
        std::fs::write(&sidecar, "{\"type\":\"user\",\"uuid\":\"s1\"}\n").expect("sidecar");
        std::fs::write(&sidecar_meta, "{\"agentId\":\"42cafe\"}\n").expect("sidecar meta");
        let sidecar_hash = sha256(&sidecar);
        let meta_hash = sha256(&sidecar_meta);

        truncate_claude_transcript_in_place(&path, "u2").expect("truncate");

        assert_eq!(sha256(&sidecar), sidecar_hash);
        assert_eq!(sha256(&sidecar_meta), meta_hash);
    }

    #[test]
    fn inplace_truncation_refuses_first_message_off_chain_and_non_user_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lines = [
            message_line("u1", None, "user", "first prompt", false),
            message_line("a1", Some("u1"), "assistant", "reply", false),
            message_line("u2a", Some("a1"), "user", "abandoned", false),
            message_line("u2b", Some("a1"), "user", "active", false),
            message_line("a2", Some("u2b"), "assistant", "done", false),
        ];
        let path = write_transcript(dir.path(), &lines);
        let before = sha256(&path);

        let first = truncate_claude_transcript_in_place(&path, "u1").expect_err("first message");
        assert!(first.contains("first message"), "{first}");
        let off_chain = truncate_claude_transcript_in_place(&path, "u2a").expect_err("off chain");
        assert!(off_chain.contains("active chain"), "{off_chain}");
        let non_user = truncate_claude_transcript_in_place(&path, "a1").expect_err("non-user");
        assert!(non_user.contains("not a user row"), "{non_user}");
        let missing = truncate_claude_transcript_in_place(&path, "zz").expect_err("missing");
        assert!(missing.contains("not found"), "{missing}");

        assert_eq!(
            sha256(&path),
            before,
            "refusals must leave the file untouched"
        );
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".bak-edit-") || name.contains(".tmp-edit-"))
            .collect();
        assert!(
            strays.is_empty(),
            "refusals must leave no strays: {strays:?}"
        );
    }
}
