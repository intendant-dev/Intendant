//! Codex anchor-fork staging: the rollout copy `thread/fork{path}` seeds
//! the child from, plus anchor→trim resolution. Copy-only over the parent
//! rollout; the staged file lives under Intendant's own state dir and is
//! swept after a retention window.

use super::ForkAnchorSpec;
use crate::managed_context_ops::{rollout_user_turns, shared_context_rewind_anchor_scan};
use std::io;
use std::path::{Path, PathBuf};

/// How the fresh forked child gets trimmed to the anchor before its first
/// turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CodexForkCut {
    /// No trim — the anchor is the parent's current head.
    None,
    /// Vanilla-safe whole-turn rollback (`thread/rollback{numTurns}`).
    Turns(u32),
    /// Managed-binary exact cut
    /// (`thread/rollback{anchor:{itemId,position}}`).
    ItemAnchor { item_id: String, position: String },
}

/// Stagings older than this are swept on the next staging call — long
/// enough for any spawn to consume its file, short enough that failed
/// forks don't accumulate multi-MB copies.
const STAGING_SWEEP_AFTER_SECS: u64 = 7 * 24 * 60 * 60;

/// Copy `source_rollout` under `staging_root` for a fork seed: byte copy
/// with a torn tail (a live parent mid-append) trimmed back to the last
/// complete, parseable line. The parent file is never written.
pub(crate) fn stage_codex_rollout_copy(
    staging_root: &Path,
    source_rollout: &Path,
) -> io::Result<PathBuf> {
    std::fs::create_dir_all(staging_root)?;
    sweep_stale_stagings(staging_root);

    let bytes = std::fs::read(source_rollout)?;
    let keep = complete_prefix_len(&bytes);
    if keep == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rollout has no complete lines to stage",
        ));
    }
    let staged = staging_root.join(format!("fork-{}.jsonl", uuid::Uuid::new_v4()));
    std::fs::write(&staged, &bytes[..keep])?;
    Ok(staged)
}

/// Byte length of the prefix ending at the last complete AND parseable
/// line (trailing partial writes and a torn final line are dropped).
fn complete_prefix_len(bytes: &[u8]) -> usize {
    let mut end = bytes.len();
    loop {
        // Trim back to (and including) the last newline within `end`.
        let line_start = match bytes[..end].iter().rposition(|b| *b == b'\n') {
            Some(last_newline) => {
                if last_newline + 1 == end {
                    // `end` sits just past a newline: the candidate line is
                    // the one BEFORE it.
                    match bytes[..last_newline].iter().rposition(|b| *b == b'\n') {
                        Some(prev) => prev + 1,
                        None => 0,
                    }
                } else {
                    // Trailing bytes after the last newline are a torn line.
                    end = last_newline + 1;
                    continue;
                }
            }
            None => return 0,
        };
        let line = &bytes[line_start..end - 1];
        if line.is_empty() || serde_json::from_slice::<serde_json::Value>(line).is_ok() {
            return end;
        }
        end = line_start;
        if end == 0 {
            return 0;
        }
    }
}

fn sweep_stale_stagings(staging_root: &Path) {
    let Ok(entries) = std::fs::read_dir(staging_root) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age.as_secs() > STAGING_SWEEP_AFTER_SECS);
        if stale {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Resolve the wire anchor to the trim the forked child needs, validated
/// against the STAGED copy (the anchor must still exist there — a parent
/// that rewound since the catalog was read must refuse, not mis-cut).
pub(crate) fn codex_anchor_turn_cut(
    staged: &Path,
    anchor: &ForkAnchorSpec,
    managed_binary: bool,
) -> Result<CodexForkCut, String> {
    let turns = rollout_user_turns(staged)
        .map_err(|err| format!("failed to scan the staged rollout: {err}"))?;
    let total = turns.len() as u32;

    match anchor.kind.as_str() {
        "head" => Ok(CodexForkCut::None),
        "turn-boundary" => {
            let keep = anchor
                .turn
                .ok_or_else(|| "a turn-boundary anchor needs a `turn`".to_string())?;
            if keep == 0 {
                return Err("a fork keeping zero turns has no value".to_string());
            }
            if keep > total {
                return Err(format!(
                    "turn {keep} not found in the staged rollout ({total} effective turns — \
                     history may have moved since the fork points were read)"
                ));
            }
            if keep == total {
                Ok(CodexForkCut::None)
            } else {
                Ok(CodexForkCut::Turns(total - keep))
            }
        }
        "item-anchor" => {
            let item_id = anchor
                .item_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| "an item anchor needs an `item_id`".to_string())?;
            let scan = shared_context_rewind_anchor_scan(staged)
                .map_err(|err| format!("failed to scan the staged rollout: {err}"))?;
            let entry = scan
                .catalog
                .iter()
                .find(|entry| entry.item_id == item_id)
                .ok_or_else(|| {
                    format!(
                        "item anchor {item_id} not found in the staged rollout \
                         (history may have moved since the fork points were read)"
                    )
                })?;
            if managed_binary {
                let position = anchor
                    .position
                    .as_deref()
                    .map(str::trim)
                    .filter(|position| matches!(*position, "before" | "after"))
                    .unwrap_or("after");
                return Ok(CodexForkCut::ItemAnchor {
                    item_id: item_id.to_string(),
                    position: position.to_string(),
                });
            }
            // Vanilla rounding: the cut lands at the boundary before the
            // anchor's containing turn (the catalog's `effective_cut`).
            let containing_turn = turns
                .iter()
                .take_while(|turn| turn.line <= entry.first_line)
                .count() as u32;
            let keep = containing_turn.saturating_sub(1);
            if keep == 0 {
                return Err(
                    "this item anchor precedes the first turn: a vanilla-binary fork would \
                     keep nothing (the managed codex binary can cut it exactly)"
                        .to_string(),
                );
            }
            Ok(CodexForkCut::Turns(total - keep))
        }
        other => Err(format!("unsupported codex fork anchor kind: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollout_lines() -> Vec<String> {
        vec![
            serde_json::json!({"timestamp":"t","type":"session_meta","payload":{"id":"0000","cwd":"/tmp"}}).to_string(),
            serde_json::json!({"timestamp":"t","type":"event_msg","payload":{"type":"user_message","message":"turn one"}}).to_string(),
            serde_json::json!({"timestamp":"t","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"a1"}]}}).to_string(),
            serde_json::json!({"timestamp":"t","type":"event_msg","payload":{"type":"user_message","message":"turn two"}}).to_string(),
            serde_json::json!({"timestamp":"t","type":"event_msg","payload":{"type":"user_message","message":"turn three"}}).to_string(),
        ]
    }

    fn write_rollout(dir: &Path, tail: &str) -> PathBuf {
        let path = dir.join("rollout-src.jsonl");
        std::fs::write(&path, rollout_lines().join("\n") + "\n" + tail).expect("write");
        path
    }

    fn anchor(kind: &str, turn: Option<u32>, item_id: Option<&str>) -> ForkAnchorSpec {
        ForkAnchorSpec {
            kind: kind.to_string(),
            turn,
            item_id: item_id.map(str::to_string),
            position: None,
            seq: None,
            message_uuid: None,
        }
    }

    #[test]
    fn staging_copies_and_trims_torn_tail_without_touching_source() {
        let dir = tempfile::tempdir().expect("dir");
        let source = write_rollout(dir.path(), "{\"torn");
        let source_bytes = std::fs::read(&source).expect("source bytes");

        let staged = stage_codex_rollout_copy(&dir.path().join("staging"), &source).expect("stage");
        let staged_text = std::fs::read_to_string(&staged).expect("staged");
        assert_eq!(staged_text.lines().count(), 5);
        assert!(!staged_text.contains("torn"));
        assert_eq!(
            std::fs::read(&source).expect("source after"),
            source_bytes,
            "source rollout mutated"
        );
    }

    #[test]
    fn staging_drops_unparseable_final_line() {
        let dir = tempfile::tempdir().expect("dir");
        let source = dir.path().join("rollout-bad.jsonl");
        std::fs::write(&source, "{\"ok\":1}\nnot json at all\n").expect("write");
        let staged = stage_codex_rollout_copy(&dir.path().join("staging"), &source).expect("stage");
        assert_eq!(
            std::fs::read_to_string(&staged).expect("staged"),
            "{\"ok\":1}\n"
        );
    }

    #[test]
    fn turn_boundary_cut_maps_keep_to_drop() {
        let dir = tempfile::tempdir().expect("dir");
        let source = write_rollout(dir.path(), "");
        assert_eq!(
            codex_anchor_turn_cut(&source, &anchor("turn-boundary", Some(1), None), false),
            Ok(CodexForkCut::Turns(2))
        );
        assert_eq!(
            codex_anchor_turn_cut(&source, &anchor("turn-boundary", Some(3), None), false),
            Ok(CodexForkCut::None)
        );
        assert!(
            codex_anchor_turn_cut(&source, &anchor("turn-boundary", Some(9), None), false).is_err()
        );
        assert!(
            codex_anchor_turn_cut(&source, &anchor("turn-boundary", Some(0), None), false).is_err()
        );
    }

    #[test]
    fn head_anchor_needs_no_trim() {
        let dir = tempfile::tempdir().expect("dir");
        let source = write_rollout(dir.path(), "");
        assert_eq!(
            codex_anchor_turn_cut(&source, &anchor("head", None, None), false),
            Ok(CodexForkCut::None)
        );
    }

    #[test]
    fn stale_item_anchor_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let source = write_rollout(dir.path(), "");
        let err = codex_anchor_turn_cut(
            &source,
            &anchor("item-anchor", None, Some("missing_item")),
            false,
        )
        .expect_err("missing anchor");
        assert!(err.contains("not found"), "{err}");
    }
}
