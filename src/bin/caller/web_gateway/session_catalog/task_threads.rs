//! Claude Code Task-tool sub-thread resolution: map a synthetic
//! `task-<sanitized toolu id>` child session id (minted by
//! `external_agent::claude_code::task_tool_child_id` when the wrapper
//! observes an in-band Task/Agent spawn) back to the subagent transcript
//! Claude Code itself persisted, so the dashboard can replay the child
//! window after the live wrapper is gone.
//!
//! Claude Code stores each sub-thread at
//! `projects/<project>/<parent-uuid>/subagents/agent-<agentId>.jsonl`
//! with a sibling `agent-<agentId>.meta.json` whose `toolUseId` is the
//! spawning tool_use id — the same correlation key the synthetic id was
//! derived from. No store keys anything by the synthetic id itself, so
//! resolution is a bounded scan over the subagent meta sidecars: dirent
//! walks plus one tiny JSON read per meta — transcripts are never opened
//! here. Codex sub-threads need no counterpart: they get first-class
//! rollout files under their own thread ids, so the ordinary finder
//! already resolves them.

use super::*;

/// Sidecar metas are a handful of short fields; anything bigger is not a
/// meta this resolver understands, and skipping it keeps the scan's read
/// volume bounded by construction.
const SUBAGENT_META_READ_LIMIT: u64 = 64 * 1024;

/// Shape gate: only ids the Task-child minting could have produced are
/// worth a store scan. `task_tool_child_id` emits `task-` plus a
/// non-empty run of ASCII alphanumerics with every other char mapped to
/// `-`, so anything else can 404 without touching the disk.
pub(crate) fn is_claude_task_child_id(session_id: &str) -> bool {
    session_id.strip_prefix("task-").is_some_and(|rest| {
        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// Resolve a synthetic `task-*` id to its `agent-<agentId>.jsonl`
/// transcript: scan `projects/*/*/subagents/*.meta.json`, matching each
/// sidecar's `toolUseId` through the SAME minting function the wrapper
/// used, and return the first match whose sibling transcript exists. A
/// relocated store can carry the parent dir under several project
/// aliases (S0b Q1); the copies describe the same thread, so any match
/// serves.
pub(crate) fn find_claude_task_subagent_transcript(
    home: &Path,
    session_id: &str,
) -> Option<PathBuf> {
    if !is_claude_task_child_id(session_id) {
        return None;
    }
    let projects = home.join(".claude").join("projects");
    for project in std::fs::read_dir(&projects).ok()?.flatten() {
        if !project.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(children) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for child in children.flatten() {
            if !child.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let subagents = child.path().join("subagents");
            let Ok(metas) = std::fs::read_dir(&subagents) else {
                continue;
            };
            for meta in metas.flatten() {
                let meta_path = meta.path();
                let Some(agent_stem) = meta_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_suffix(".meta.json"))
                    .filter(|stem| stem.starts_with("agent-"))
                else {
                    continue;
                };
                if meta
                    .metadata()
                    .map(|m| m.len() > SUBAGENT_META_READ_LIMIT)
                    .unwrap_or(true)
                {
                    continue;
                }
                if !subagent_meta_matches_task_id(&meta_path, session_id) {
                    continue;
                }
                let transcript = subagents.join(format!("{agent_stem}.jsonl"));
                if transcript.is_file() {
                    return Some(transcript);
                }
            }
        }
    }
    None
}

/// One sidecar read: does this meta's `toolUseId` mint the requested id?
fn subagent_meta_matches_task_id(meta_path: &Path, session_id: &str) -> bool {
    let Ok(contents) = std::fs::read_to_string(meta_path) else {
        return false;
    };
    let Ok(meta) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return false;
    };
    meta.get("toolUseId")
        .and_then(|value| value.as_str())
        .is_some_and(|tool_use_id| {
            crate::external_agent::claude_code::task_tool_child_id(tool_use_id) == session_id
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARENT: &str = "38764863-4398-42bb-9d59-3ffdddee5000";
    const TOOL_USE_ID: &str = "toolu_01AAABBBCCCDDDEEE";
    const TASK_ID: &str = "task-AAABBBCCCDDDEEE";

    fn write_subagent(
        home: &Path,
        project: &str,
        parent: &str,
        agent: &str,
        tool_use_id: &str,
        transcript_lines: &[serde_json::Value],
    ) -> PathBuf {
        let subagents = home
            .join(".claude")
            .join("projects")
            .join(project)
            .join(parent)
            .join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(
            subagents.join(format!("agent-{agent}.meta.json")),
            serde_json::json!({
                "agentType": "general-purpose",
                "description": "content-aware turn alignment",
                "toolUseId": tool_use_id,
                "spawnDepth": 1,
            })
            .to_string(),
        )
        .unwrap();
        let transcript = subagents.join(format!("agent-{agent}.jsonl"));
        let mut body = transcript_lines
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        body.push('\n');
        std::fs::write(&transcript, body).unwrap();
        transcript
    }

    /// A sidechain transcript record (subagent-file shape: `sessionId`
    /// is the PARENT uuid; every record carries the sidechain facts).
    fn sidechain_line(
        record_type: &str,
        uuid: &str,
        ts: &str,
        content: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "parentUuid": null,
            "isSidechain": true,
            "agentId": "a0343c7095a040965",
            "type": record_type,
            "message": { "role": record_type, "content": content },
            "uuid": uuid,
            "timestamp": ts,
            "sessionId": PARENT,
            "version": "2.1.207",
        })
    }

    fn fixture_lines() -> Vec<serde_json::Value> {
        vec![
            sidechain_line(
                "user",
                "u-1",
                "2026-07-17T10:00:00.000Z",
                serde_json::json!("Align the transcript turns"),
            ),
            sidechain_line(
                "assistant",
                "a-1",
                "2026-07-17T10:00:05.000Z",
                serde_json::json!([{ "type": "text", "text": "Starting on the alignment." }]),
            ),
        ]
    }

    #[test]
    fn task_child_id_shape_gate() {
        assert!(is_claude_task_child_id(TASK_ID));
        assert!(is_claude_task_child_id("task-9Qo6orCDCTgHgt1iBvztac"));
        assert!(is_claude_task_child_id("task-weird-id"));
        assert!(!is_claude_task_child_id("task-"));
        assert!(!is_claude_task_child_id("task"));
        assert!(!is_claude_task_child_id(
            "38764863-4398-42bb-9d59-3ffdddee5000"
        ));
        assert!(!is_claude_task_child_id("task-abc/def"));
        assert!(!is_claude_task_child_id("codex-thread-1"));
    }

    #[test]
    fn resolves_minted_id_to_sibling_transcript() {
        let home = tempfile::tempdir().unwrap();
        let transcript = write_subagent(
            home.path(),
            "-repo-project",
            PARENT,
            "a0343c7095a040965",
            TOOL_USE_ID,
            &fixture_lines(),
        );
        // The id under test is minted by the SAME function the wrapper
        // uses, so this test can never drift from the live ids.
        let minted = crate::external_agent::claude_code::task_tool_child_id(TOOL_USE_ID);
        assert_eq!(minted, TASK_ID);
        assert_eq!(
            find_claude_task_subagent_transcript(home.path(), &minted),
            Some(transcript)
        );
    }

    #[test]
    fn unmatched_ids_resolve_to_none() {
        let home = tempfile::tempdir().unwrap();
        write_subagent(
            home.path(),
            "-repo-project",
            PARENT,
            "a0343c7095a040965",
            TOOL_USE_ID,
            &fixture_lines(),
        );
        // A task-shaped id no meta mints stays a miss (the 404 today).
        assert_eq!(
            find_claude_task_subagent_transcript(home.path(), "task-NOSUCHTOOLUSE1"),
            None
        );
        // Non-task ids never scan.
        assert_eq!(
            find_claude_task_subagent_transcript(home.path(), PARENT),
            None
        );
    }

    #[test]
    fn skips_malformed_metas_and_requires_sibling_transcript() {
        let home = tempfile::tempdir().unwrap();
        let subagents = home
            .path()
            .join(".claude")
            .join("projects")
            .join("-repo-project")
            .join(PARENT)
            .join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        // Malformed meta, meta without toolUseId, and a matching meta
        // whose transcript is missing: all skipped without error.
        std::fs::write(subagents.join("agent-bad.meta.json"), "not json").unwrap();
        std::fs::write(
            subagents.join("agent-none.meta.json"),
            serde_json::json!({"agentType": "Explore"}).to_string(),
        )
        .unwrap();
        std::fs::write(
            subagents.join("agent-orphan.meta.json"),
            serde_json::json!({"toolUseId": TOOL_USE_ID}).to_string(),
        )
        .unwrap();
        assert_eq!(
            find_claude_task_subagent_transcript(home.path(), TASK_ID),
            None
        );
        // A later well-formed sibling still resolves past the noise.
        let transcript = write_subagent(
            home.path(),
            "-repo-project",
            PARENT,
            "z9999",
            TOOL_USE_ID,
            &fixture_lines(),
        );
        assert_eq!(
            find_claude_task_subagent_transcript(home.path(), TASK_ID),
            Some(transcript)
        );
    }

    #[test]
    fn detail_page_serves_task_child_entries() {
        let home = tempfile::tempdir().unwrap();
        write_subagent(
            home.path(),
            "-repo-project",
            PARENT,
            "a0343c7095a040965",
            TOOL_USE_ID,
            &fixture_lines(),
        );
        let body = external_session_detail_from_home_with_page(
            home.path(),
            "claude-code",
            TASK_ID,
            None,
            None,
        )
        .expect("task child should resolve through the claude-code detail path");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            value.get("session_id").and_then(|v| v.as_str()),
            Some(TASK_ID)
        );
        let entries = value
            .get("entries")
            .and_then(|v| v.as_array())
            .expect("entries array");
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].get("content").and_then(|v| v.as_str()),
            Some("Align the transcript turns")
        );
        assert_eq!(
            entries[0].get("source").and_then(|v| v.as_str()),
            Some("User")
        );
        assert_eq!(
            entries[1].get("content").and_then(|v| v.as_str()),
            Some("Starting on the alignment.")
        );

        // Non-matching task ids keep today's 404 contract.
        assert_eq!(
            external_session_detail_from_home_with_page(
                home.path(),
                "claude-code",
                "task-NOSUCHTOOLUSE1",
                None,
                None,
            ),
            None
        );
    }
}
