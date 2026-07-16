//! Native-session fork engine: materialize a NEW session whose
//! conversation is the parent's prefix through a chosen `seq`. Copy-only —
//! the parent's log dir is read, never written; the child gets its own
//! uuid log dir with a truncated `conversation.jsonl`, a rewritten
//! `session_meta.json`, and a durable `session_relationship` line so the
//! lineage ledger sees the `anchor-fork` edge before the child ever runs.

use super::ForkAnchorSpec;
use std::io::Write;
use std::path::Path;

#[derive(Debug)]
pub(crate) struct NativeForkOutcome {
    pub(crate) child_session_id: String,
    pub(crate) kept_messages: usize,
}

/// Load-time window for the fork's conversation read. Budget math is not
/// exercised here — the child re-loads under its provider's real window
/// when it resumes.
const FORK_LOAD_CONTEXT_WINDOW: u64 = 1_000_000;

pub(crate) fn fork_native_session_at_seq(
    logs_root: &Path,
    parent_session_id: &str,
    parent_log_dir: &Path,
    anchor: &ForkAnchorSpec,
    child_name: Option<&str>,
) -> Result<NativeForkOutcome, String> {
    let cut_after_seq = anchor
        .seq
        .filter(|seq| *seq > 0)
        .ok_or_else(|| "a native fork anchor needs a non-zero `seq`".to_string())?;
    let conversation_path = parent_log_dir.join("conversation.jsonl");
    let mut conversation = crate::conversation::Conversation::load_from_file(
        &conversation_path,
        FORK_LOAD_CONTEXT_WINDOW,
    )
    .map_err(|err| format!("failed to load the parent conversation: {err}"))?;
    let keep = conversation
        .prefix_len_through_seq(cut_after_seq)
        .ok_or_else(|| {
            format!(
                "anchor seq {cut_after_seq} not found in the parent conversation \
             (history may have moved since the fork points were read)"
            )
        })?;
    conversation.truncate_to(keep);

    let child_session_id = uuid::Uuid::new_v4().to_string();
    let child_log_dir = logs_root.join(&child_session_id);
    std::fs::create_dir_all(&child_log_dir)
        .map_err(|err| format!("failed to create the child log dir: {err}"))?;
    conversation
        .save_to_file(&child_log_dir.join("conversation.jsonl"))
        .map_err(|err| format!("failed to write the child conversation: {err}"))?;

    // The child's meta starts as the parent's (project root, provider…)
    // with its own identity and fork provenance.
    let mut meta = std::fs::read_to_string(parent_log_dir.join("session_meta.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    meta["session_id"] = serde_json::Value::String(child_session_id.clone());
    meta["forked_from"] = serde_json::Value::String(parent_session_id.to_string());
    meta["fork_cut_after_seq"] = serde_json::Value::from(cut_after_seq);
    if let Some(name) = child_name.map(str::trim).filter(|name| !name.is_empty()) {
        meta["name"] = serde_json::Value::String(name.to_string());
    }
    std::fs::write(child_log_dir.join("session_meta.json"), meta.to_string())
        .map_err(|err| format!("failed to write the child session meta: {err}"))?;

    // Durable lineage edge in the child's own spine, in the exact
    // `session_relationship` shape the lineage ledger folds (the test
    // below pins that via `read_lineage_ledger`).
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let line = serde_json::json!({
        "ts": chrono::Local::now().format("%H:%M:%S%.3f").to_string(),
        "ts_ms": now_ms,
        "event": "session_relationship",
        "message": format!("anchor-fork of {parent_session_id} (seq {cut_after_seq})"),
        "data": {
            "parent_session_id": parent_session_id,
            "child_session_id": child_session_id,
            "relationship": "anchor-fork",
            "ephemeral": false,
        },
    });
    let mut spine = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(child_log_dir.join("session.jsonl"))
        .map_err(|err| format!("failed to open the child session log: {err}"))?;
    writeln!(spine, "{line}").map_err(|err| format!("failed to write the lineage edge: {err}"))?;
    spine
        .sync_all()
        .map_err(|err| format!("failed to sync the child session log: {err}"))?;

    Ok(NativeForkOutcome {
        child_session_id,
        kept_messages: keep,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;
    use std::path::PathBuf;

    fn seed_parent(logs_root: &Path, session_id: &str) -> PathBuf {
        let dir = logs_root.join(session_id);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let lines = [
            serde_json::json!({"role":"system","content":"sys","seq":1}),
            serde_json::json!({"role":"user","content":"round one","seq":2}),
            serde_json::json!({"role":"assistant","content":"answer one","seq":3}),
            serde_json::json!({"role":"user","content":"round two","seq":4}),
            serde_json::json!({"role":"assistant","content":"answer two","seq":5}),
        ];
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(dir.join("conversation.jsonl"), body).expect("conversation");
        std::fs::write(
            dir.join("session_meta.json"),
            serde_json::json!({"session_id": session_id, "project_root": "/tmp/proj"}).to_string(),
        )
        .expect("meta");
        dir
    }

    fn dir_hash(dir: &Path) -> String {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .expect("read dir")
            .flatten()
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        let mut hasher = sha2::Sha256::new();
        for path in entries {
            hasher.update(path.to_string_lossy().as_bytes());
            hasher.update(std::fs::read(&path).expect("read file"));
        }
        format!("{:x}", hasher.finalize())
    }

    fn anchor(seq: u64) -> ForkAnchorSpec {
        ForkAnchorSpec {
            kind: "round".to_string(),
            turn: None,
            item_id: None,
            position: None,
            seq: Some(seq),
            message_uuid: None,
        }
    }

    #[test]
    fn fork_materializes_truncated_child_and_leaves_parent_untouched() {
        let root = tempfile::tempdir().expect("root");
        let parent_dir = seed_parent(root.path(), "parent-id");
        let parent_before = dir_hash(&parent_dir);

        let outcome =
            fork_native_session_at_seq(root.path(), "parent-id", &parent_dir, &anchor(3), None)
                .expect("fork");
        assert_eq!(outcome.kept_messages, 3);
        assert_eq!(dir_hash(&parent_dir), parent_before, "parent mutated");
        let child_log_dir = root.path().join(&outcome.child_session_id);

        let child_conversation = std::fs::read_to_string(child_log_dir.join("conversation.jsonl"))
            .expect("child conversation");
        assert_eq!(child_conversation.lines().count(), 3);
        assert!(child_conversation.contains("answer one"));
        assert!(!child_conversation.contains("round two"));

        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(child_log_dir.join("session_meta.json")).expect("child meta"),
        )
        .expect("meta json");
        assert_eq!(meta["session_id"], outcome.child_session_id.as_str());
        assert_eq!(meta["forked_from"], "parent-id");
        assert_eq!(meta["project_root"], "/tmp/proj");

        // The lineage ledger folds the edge from the child's spine.
        let ledger = crate::lineage_ledger::read_lineage_ledger(&child_log_dir, "parent-id")
            .expect("ledger read")
            .expect("ledger present");
        let branch = ledger
            .groups
            .iter()
            .flat_map(|group| group.branches.iter())
            .find(|branch| branch.session_id == outcome.child_session_id)
            .expect("anchor-fork branch");
        assert_eq!(branch.relationship, "anchor-fork");
    }

    #[test]
    fn fork_names_the_child_when_asked() {
        let root = tempfile::tempdir().expect("root");
        let parent_dir = seed_parent(root.path(), "parent-id");
        let outcome = fork_native_session_at_seq(
            root.path(),
            "parent-id",
            &parent_dir,
            &anchor(2),
            Some("experiment"),
        )
        .expect("fork");
        let child_log_dir = root.path().join(&outcome.child_session_id);
        let meta: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(child_log_dir.join("session_meta.json")).expect("child meta"),
        )
        .expect("meta json");
        assert_eq!(meta["name"], "experiment");
    }

    #[test]
    fn stale_or_zero_seq_is_refused() {
        let root = tempfile::tempdir().expect("root");
        let parent_dir = seed_parent(root.path(), "parent-id");
        let err =
            fork_native_session_at_seq(root.path(), "parent-id", &parent_dir, &anchor(99), None)
                .expect_err("stale seq");
        assert!(err.contains("not found"));
        let err = fork_native_session_at_seq(
            root.path(),
            "parent-id",
            &parent_dir,
            &ForkAnchorSpec {
                kind: "round".to_string(),
                turn: None,
                item_id: None,
                position: None,
                seq: Some(0),
                message_uuid: None,
            },
            None,
        )
        .expect_err("zero seq");
        assert!(err.contains("non-zero"));
    }
}
