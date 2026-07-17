//! Claude Code anchor-fork surgery: materialize a NEW transcript in the
//! parent's project dir whose content is the CHAIN-SLICE through the
//! anchor — the anchor's ancestor chain plus the uuid-less meta lines, in
//! original order, with every line's `sessionId` rewritten to the child's
//! fresh uuid. Copy-only: the parent transcript is never written.
//!
//! Semantics pinned by `tests/skills/session-fork-probes/spike2` (CC
//! 2.1.211): resume resolves by filename stem and walks the chain back
//! from the tail, so the slice makes the anchor the child's tail;
//! `compact_boundary` lines sever the walk, and a slice through a
//! pre-boundary anchor simply never includes the boundary — pre-compaction
//! anchors fork with full history and normal auto-compaction.

use super::{parse_claude_transcript_tree_from_lines, ForkAnchorSpec};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct ClaudeForkPlan {
    pub(crate) parent_path: PathBuf,
    pub(crate) child_uuid: String,
    pub(crate) child_path: PathBuf,
    pub(crate) anchor_uuid: String,
}

pub(crate) struct ClaudeForkOutcome {
    pub(crate) kept_lines: usize,
}

/// Validate the anchor against the parent transcript and mint the child
/// identity. Refuses unknown and sidechain anchors.
pub(crate) fn plan_claude_fork(
    transcript: &Path,
    anchor: &ForkAnchorSpec,
) -> Result<ClaudeForkPlan, String> {
    let anchor_uuid = anchor
        .message_uuid
        .as_deref()
        .map(str::trim)
        .filter(|uuid| !uuid.is_empty())
        .ok_or_else(|| "a claude-code fork anchor needs a `message_uuid`".to_string())?;
    let tree = super::shared_claude_tree_scan(transcript)
        .map_err(|err| format!("failed to scan the parent transcript: {err}"))?;
    let node = tree.node(anchor_uuid).ok_or_else(|| {
        format!(
            "anchor message {anchor_uuid} not found in the parent transcript \
             (history may have moved since the fork points were read)"
        )
    })?;
    if node.is_sidechain {
        return Err(format!(
            "anchor message {anchor_uuid} is sidechain work — fork from a main-chain anchor"
        ));
    }
    let parent_dir = transcript
        .parent()
        .ok_or_else(|| "parent transcript has no project dir".to_string())?;
    let child_uuid = uuid::Uuid::new_v4().to_string();
    let child_path = parent_dir.join(format!("{child_uuid}.jsonl"));
    Ok(ClaudeForkPlan {
        parent_path: transcript.to_path_buf(),
        child_uuid,
        child_path,
        anchor_uuid: anchor_uuid.to_string(),
    })
}

/// Execute the chain-slice copy. The slice and the emitted lines come from
/// ONE read of the parent, so a live parent appending mid-fork cannot skew
/// the cut (new tail lines simply aren't part of the read).
pub(crate) fn execute_claude_fork_copy(plan: &ClaudeForkPlan) -> Result<ClaudeForkOutcome, String> {
    let raw = std::fs::read_to_string(&plan.parent_path)
        .map_err(|err| format!("failed to read the parent transcript: {err}"))?;
    let lines: Vec<&str> = raw.lines().collect();
    let tree = parse_claude_transcript_tree_from_lines(lines.iter().copied());
    let chain: HashSet<String> = tree
        .ancestor_chain(&plan.anchor_uuid)
        .iter()
        .map(|node| node.uuid.clone())
        .collect();
    if chain.is_empty() {
        return Err(format!(
            "anchor message {} not found in the parent transcript \
             (history may have moved since the fork points were read)",
            plan.anchor_uuid
        ));
    }

    let mut kept = 0usize;
    let mut body = String::new();
    for line in &lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue; // torn tail / foreign line
        };
        let on_chain = match value.get("uuid").and_then(serde_json::Value::as_str) {
            Some(uuid) => chain.contains(uuid),
            None => true, // uuid-less meta lines (ai-title, agent-name, …)
        };
        if !on_chain {
            continue;
        }
        if let Some(object) = value.as_object_mut() {
            if object.contains_key("sessionId") {
                object.insert(
                    "sessionId".to_string(),
                    serde_json::Value::String(plan.child_uuid.clone()),
                );
            }
        }
        body.push_str(&value.to_string());
        body.push('\n');
        kept += 1;
    }

    let mut file = std::fs::File::create(&plan.child_path)
        .map_err(|err| format!("failed to create the child transcript: {err}"))?;
    file.write_all(body.as_bytes())
        .map_err(|err| format!("failed to write the child transcript: {err}"))?;
    file.sync_all()
        .map_err(|err| format!("failed to sync the child transcript: {err}"))?;
    Ok(ClaudeForkOutcome { kept_lines: kept })
}

#[cfg(test)]
mod tests {
    use super::super::test_fixtures::{boundary_line, message_line};
    use super::*;
    use sha2::Digest;

    fn anchor(uuid: &str) -> ForkAnchorSpec {
        ForkAnchorSpec {
            kind: "message".to_string(),
            turn: None,
            item_id: None,
            position: None,
            seq: None,
            message_uuid: Some(uuid.to_string()),
        }
    }

    fn write_parent(lines: &[String]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir
            .path()
            .join("aaaaaaaa-0000-0000-0000-000000000000.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").expect("write");
        (dir, path)
    }

    fn file_hash(path: &Path) -> String {
        format!(
            "{:x}",
            sha2::Sha256::digest(std::fs::read(path).expect("read"))
        )
    }

    fn fork(parent: &Path, anchor_uuid: &str) -> (ClaudeForkPlan, ClaudeForkOutcome) {
        let plan = plan_claude_fork(parent, &anchor(anchor_uuid)).expect("plan");
        let outcome = execute_claude_fork_copy(&plan).expect("execute");
        (plan, outcome)
    }

    #[test]
    fn chain_slice_keeps_ancestors_and_meta_drops_siblings_and_descendants() {
        let meta = "{\"type\":\"ai-title\",\"aiTitle\":\"t\",\"sessionId\":\"aaaaaaaa-0000-0000-0000-000000000000\"}".to_string();
        let (_dir, parent) = write_parent(&[
            meta,
            message_line("u1", None, "user", "round one", false),
            message_line("a1", Some("u1"), "assistant", "answer one", false),
            message_line("a1b", Some("u1"), "assistant", "abandoned sibling", false),
            message_line("u2", Some("a1"), "user", "round two", false),
        ]);
        let parent_before = file_hash(&parent);

        let (plan, outcome) = fork(&parent, "a1");
        assert_eq!(outcome.kept_lines, 3); // meta + u1 + a1
        assert_eq!(file_hash(&parent), parent_before, "parent mutated");

        let child = std::fs::read_to_string(&plan.child_path).expect("child");
        assert!(child.contains("answer one"));
        assert!(!child.contains("abandoned sibling"));
        assert!(!child.contains("round two"));
        // Every line, including meta, carries the child's session id.
        for line in child.lines() {
            let value: serde_json::Value = serde_json::from_str(line).expect("child line json");
            if let Some(session_id) = value.get("sessionId") {
                assert_eq!(session_id.as_str(), Some(plan.child_uuid.as_str()));
            }
        }
        assert_eq!(
            plan.child_path.file_stem().and_then(|stem| stem.to_str()),
            Some(plan.child_uuid.as_str())
        );
    }

    #[test]
    fn branch_tip_fork_keeps_that_branch() {
        let (_dir, parent) = write_parent(&[
            message_line("u1", None, "user", "root", false),
            message_line("a1", Some("u1"), "assistant", "branch A", false),
            message_line("a2", Some("u1"), "assistant", "branch B", false),
        ]);
        let (plan, outcome) = fork(&parent, "a1");
        assert_eq!(outcome.kept_lines, 2);
        let child = std::fs::read_to_string(&plan.child_path).expect("child");
        assert!(child.contains("branch A"));
        assert!(!child.contains("branch B"));
    }

    #[test]
    fn pre_boundary_slice_omits_the_compact_boundary() {
        let (_dir, parent) = write_parent(&[
            message_line("u1", None, "user", "old round", false),
            message_line("a1", Some("u1"), "assistant", "old answer", false),
            boundary_line("b1", "a1"),
            message_line("u2", Some("b1"), "user", "post-compact", false),
        ]);
        let (plan, _) = fork(&parent, "a1");
        let child = std::fs::read_to_string(&plan.child_path).expect("child");
        assert!(!child.contains("compact_boundary"));
        assert!(!child.contains("post-compact"));
        assert!(child.contains("old answer"));
    }

    #[test]
    fn unknown_and_sidechain_anchors_refuse() {
        let (_dir, parent) = write_parent(&[
            message_line("u1", None, "user", "main", false),
            message_line("s1", Some("u1"), "assistant", "sidechain", true),
        ]);
        assert!(plan_claude_fork(&parent, &anchor("missing"))
            .expect_err("unknown")
            .contains("not found"));
        assert!(plan_claude_fork(&parent, &anchor("s1"))
            .expect_err("sidechain")
            .contains("sidechain"));
    }
}
