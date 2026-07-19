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
//! already resolves them (their completed-terminal synthesis lives in
//! the ordinary codex parse — see `parse_codex_session_entries`).

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

/// What a synthetic `task-*` id resolves to in the Claude Code store: the
/// subagent transcript itself, the spawning `toolUseId` the id was minted
/// from, and — when the same project alias carries it — the PARENT
/// thread's transcript, the only file Claude Code persists the child's
/// terminal status into (`<task-notification>` records; the subagent
/// transcript just ends at its final assistant message).
pub(crate) struct ClaudeTaskSubagentArtifacts {
    pub(crate) transcript: PathBuf,
    pub(crate) tool_use_id: String,
    /// The subagent's `agentId` — the `agent-<id>` file stem without its
    /// prefix. Every `<task-notification>` vintage carries it as
    /// `<task-id>` (the spawn `<tool-use-id>` is newer and, for a child
    /// interrupted and re-run, each later run notifies under a NEW
    /// tool_use id), so this is the stable terminal-evidence key.
    pub(crate) agent_id: String,
    pub(crate) parent_transcript: Option<PathBuf>,
}

/// Resolve a synthetic `task-*` id to its `agent-<agentId>.jsonl`
/// transcript (see [`find_claude_task_subagent_artifacts`]).
pub(crate) fn find_claude_task_subagent_transcript(
    home: &Path,
    session_id: &str,
) -> Option<PathBuf> {
    find_claude_task_subagent_artifacts(home, session_id).map(|artifacts| artifacts.transcript)
}

/// Resolve a synthetic `task-*` id to its subagent-store artifacts: scan
/// `projects/*/*/subagents/*.meta.json`, matching each sidecar's
/// `toolUseId` through the SAME minting function the wrapper used, and
/// return the first match whose sibling transcript exists. A relocated
/// store can carry the parent dir under several project aliases (S0b Q1);
/// the copies describe the same thread, so any match serves — but prefer
/// an alias that also carries the parent transcript (the terminal-status
/// source), falling back to the first transcript-only match so
/// resolution never regresses to a miss.
pub(crate) fn find_claude_task_subagent_artifacts(
    home: &Path,
    session_id: &str,
) -> Option<ClaudeTaskSubagentArtifacts> {
    if !is_claude_task_child_id(session_id) {
        return None;
    }
    let projects = home.join(".claude").join("projects");
    let mut transcript_only: Option<ClaudeTaskSubagentArtifacts> = None;
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
                let Some(tool_use_id) = subagent_meta_task_tool_use_id(&meta_path, session_id)
                else {
                    continue;
                };
                let transcript = subagents.join(format!("{agent_stem}.jsonl"));
                if !transcript.is_file() {
                    continue;
                }
                // The parent transcript is the child dir's sibling
                // `<parent-uuid>.jsonl` in the same project alias.
                let parent_transcript = child
                    .path()
                    .parent()
                    .map(|dir| dir.join(format!("{}.jsonl", child.file_name().to_string_lossy())))
                    .filter(|path| path.is_file());
                let artifacts = ClaudeTaskSubagentArtifacts {
                    transcript,
                    tool_use_id,
                    agent_id: agent_stem
                        .strip_prefix("agent-")
                        .unwrap_or(agent_stem)
                        .to_string(),
                    parent_transcript,
                };
                if artifacts.parent_transcript.is_some() {
                    return Some(artifacts);
                }
                if transcript_only.is_none() {
                    transcript_only = Some(artifacts);
                }
            }
        }
    }
    transcript_only
}

/// One sidecar read: when this meta's `toolUseId` mints the requested id,
/// return that `toolUseId` (the parent-transcript correlation key).
fn subagent_meta_task_tool_use_id(meta_path: &Path, session_id: &str) -> Option<String> {
    let contents = std::fs::read_to_string(meta_path).ok()?;
    let meta = serde_json::from_str::<serde_json::Value>(&contents).ok()?;
    meta.get("toolUseId")
        .and_then(|value| value.as_str())
        .filter(|tool_use_id| {
            crate::external_agent::claude_code::task_tool_child_id(tool_use_id) == session_id
        })
        .map(str::to_string)
}

// ---------------------------------------------------------------------
// Completed-terminal synthesis
//
// The live "Task complete" LogEntry for a Task child is bus-only (never
// persisted into any session log), and the subagent transcript carries
// no terminal marker — so after a tab reload or daemon restart a
// completed child rehydrated from this store read IDLE forever. Claude
// Code itself DOES persist the completion: the parent transcript records
// each `system:task_notification` as a `<task-notification>` XML block
// (a `queue-operation` record plus the injected `user` delivery, both
// carrying `<tool-use-id>` and `<status>`). Derive a synthetic terminal
// row from that evidence so every hydration/replay lane serves the same
// DONE the live window showed.
// ---------------------------------------------------------------------

/// `kind` marker of the synthesized completed-terminal row — shared by
/// this Claude Code lane and the Codex twin
/// (`codex_subagent_terminal_entry` in `transcripts.rs`). The dashboard
/// counts it as done-evidence during hydration and dedupes it against the
/// live "Task complete" log line (which carries a different timestamp and
/// event id, so signature dedupe never pairs them).
pub(crate) const SUBAGENT_TERMINAL_KIND: &str = "subagent_terminal";

const TASK_NOTIFICATION_OPEN: &str = "<task-notification>";
const TASK_NOTIFICATION_CLOSE: &str = "</task-notification>";

/// One `<task-notification>` block's terminal facts as persisted in the
/// parent transcript, stamped with the carrying record's timestamp.
struct ClaudeTaskNotification {
    status: String,
    summary: Option<String>,
    ts: Option<String>,
    ts_ms: Option<i64>,
}

/// The LAST `<task-notification>` for this child in the parent transcript
/// ("the same task-id may notify more than once" — a resumed child
/// re-notifies, so recency wins). A block counts when it carries EITHER
/// correlation key: the spawn `<tool-use-id>` needle, or the
/// `<task-id>{agent_id}</task-id>` needle — a child interrupted and
/// re-run notifies each later run under a NEW tool_use id (and an
/// earlier-segment spawn's toolu may not be in this file at all), while
/// the `<task-id>` = agentId is present in every notification vintage.
/// Prose mentions of the raw ids never match the full-tag needles. One
/// bounded pass: lines are only JSON-parsed when they contain a needle,
/// and only the two record shapes Claude Code writes the block into are
/// accepted — an assistant message QUOTING the block must not count as
/// evidence.
fn last_claude_task_notification(
    parent_transcript: &Path,
    tool_use_id: &str,
    agent_id: &str,
) -> Option<ClaudeTaskNotification> {
    use std::io::BufRead as _;
    let spawn_needle = format!("<tool-use-id>{tool_use_id}</tool-use-id>");
    let task_needle = format!("<task-id>{agent_id}</task-id>");
    let file = std::fs::File::open(parent_transcript).ok()?;
    let reader = std::io::BufReader::new(file);
    let mut last = None;
    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        if !line.contains(&spawn_needle) && !line.contains(&task_needle) {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let text = match record.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "queue-operation" => record
                .get("content")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            "user" => claude_user_record_text(&record),
            _ => None,
        };
        let Some(text) = text else {
            continue;
        };
        // Either needle locates the block; the spawn toolu is tried first
        // (the exact-vintage key), and a needle present in the text but
        // outside any `<task-notification>` block yields no facts, so the
        // fallback still lands on the tagged block when one exists.
        let Some((status, summary)) = task_notification_facts(&text, &spawn_needle)
            .or_else(|| task_notification_facts(&text, &task_needle))
        else {
            continue;
        };
        let ts = record
            .get("timestamp")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let ts_ms = ts
            .as_deref()
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
            .map(|dt| dt.timestamp_millis());
        last = Some(ClaudeTaskNotification {
            status,
            summary,
            ts,
            ts_ms,
        });
    }
    last
}

/// A `user` record's textual content: the injected notification delivery
/// uses a plain string, but tolerate the block form too.
fn claude_user_record_text(record: &serde_json::Value) -> Option<String> {
    let content = record.get("message")?.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let blocks = content.as_array()?;
    let mut text = String::new();
    for block in blocks {
        if block.get("type").and_then(|v| v.as_str()) == Some("text") {
            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                text.push_str(t);
                text.push('\n');
            }
        }
    }
    (!text.is_empty()).then_some(text)
}

/// `<status>` / `<summary>` from the notification block that contains
/// `needle`, tolerating unrelated blocks on the same record.
fn task_notification_facts(text: &str, needle: &str) -> Option<(String, Option<String>)> {
    let needle_at = text.find(needle)?;
    let block_start = text[..needle_at].rfind(TASK_NOTIFICATION_OPEN)?;
    let block_end = text[needle_at..]
        .find(TASK_NOTIFICATION_CLOSE)
        .map(|offset| needle_at + offset)
        .unwrap_or(text.len());
    let block = &text[block_start..block_end];
    let status = xml_tag_text(block, "status")?;
    let summary = xml_tag_text(block, "summary");
    Some((status, summary))
}

fn xml_tag_text(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&close)? + start;
    let text = block[start..end].trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// One-line, bounded summary — the same shaping the live emit applies
/// (`claude_code::task_summary_snippet`), so hydrated rows read like the
/// live "Task complete" line byte-for-byte. Shared with the Codex
/// terminal synthesis (`codex_subagent_terminal_entry`), which bounds the
/// child's full `last_agent_message` the same way.
pub(crate) fn task_terminal_summary_snippet(text: &str) -> Option<String> {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() > 240 {
        Some(format!(
            "{}…",
            trimmed.chars().take(240).collect::<String>()
        ))
    } else {
        Some(trimmed.to_string())
    }
}

/// The synthesized completed-terminal row for a Task child, or None when
/// the durable evidence doesn't support one:
///
/// - no parent transcript at the resolved alias (conservative miss);
/// - the last notification is not a completion — errored / stopped
///   children keep today's behavior (their live path already emits a
///   persisted `SessionEnded`); only completion is starved of durable
///   evidence;
/// - the subagent transcript has rows NEWER than the notification (a
///   resumed child) — completion evidence older than the transcript tail
///   is stale, and the resumed run's own terminal will re-derive.
pub(crate) fn claude_task_terminal_entry(
    artifacts: &ClaudeTaskSubagentArtifacts,
    entries: &[serde_json::Value],
) -> Option<serde_json::Value> {
    let parent = artifacts.parent_transcript.as_deref()?;
    let notification =
        last_claude_task_notification(parent, &artifacts.tool_use_id, &artifacts.agent_id)?;
    // The wire statuses the live path folds into "completed"
    // (claude_code::handle_task_notification).
    if !matches!(notification.status.as_str(), "completed" | "success") {
        return None;
    }
    if let (Some(terminal_ms), Some(last_ms)) = (notification.ts_ms, last_entry_ts_ms(entries)) {
        if terminal_ms < last_ms {
            return None;
        }
    }
    // Live grammar: `emit_external_subagent_state`'s completed arm with
    // the backend source as the label.
    let label = crate::external_agent::AgentBackend::ClaudeCode.to_string();
    let content = match notification
        .summary
        .as_deref()
        .and_then(task_terminal_summary_snippet)
    {
        Some(summary) => format!("Task complete: {label} subagent completed: {summary}"),
        None => format!("Task complete: {label} subagent completed"),
    };
    let mut entry = serde_json::json!({
        "level": "info",
        "source": label,
        "kind": SUBAGENT_TERMINAL_KIND,
        "content": content,
    });
    if let Some(ts) = notification.ts {
        entry["ts"] = serde_json::Value::String(ts);
    }
    if let Some(ts_ms) = notification.ts_ms {
        entry["ts_ms"] = serde_json::Value::from(ts_ms);
    }
    Some(entry)
}

fn last_entry_ts_ms(entries: &[serde_json::Value]) -> Option<i64> {
    entries
        .iter()
        .rev()
        .find_map(|entry| entry.get("ts_ms").and_then(|v| v.as_i64()))
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

    // ---- Completed-terminal synthesis ----

    /// The block Claude Code persists into the parent transcript for a
    /// `system:task_notification` (fields in observed live order; the
    /// multi-line `<result>` proves summary extraction doesn't bleed).
    fn notification_block(tool_use_id: &str, status: &str, summary: &str) -> String {
        format!(
            "<task-notification>\n<task-id>a0343c7095a040965</task-id>\n\
             <tool-use-id>{tool_use_id}</tool-use-id>\n\
             <output-file>/tmp/tasks/a0343c7095a040965.output</output-file>\n\
             <status>{status}</status>\n<summary>{summary}</summary>\n\
             <note>A task-notification fires each time this agent stops.</note>\n\
             <result>Full report:\n\n## Details\n\nline two</result>\n\
             </task-notification>"
        )
    }

    /// A `<task-notification>` block as persisted for a RE-RUN of an
    /// interrupted child: Claude Code stamps it with the re-run turn's
    /// NEW tool_use id — the spawn toolu appears nowhere in it — while
    /// the `<task-id>` stays the stable agentId (live specimen class:
    /// task-KCC, 2026-07-18).
    fn rerun_notification_block(rerun_tool_use_id: &str, status: &str, summary: &str) -> String {
        notification_block(rerun_tool_use_id, status, summary)
    }

    /// The `<task-id>`-only vintage (also the earlier-segment shape,
    /// where the spawn toolu is not in the current parent segment at
    /// all): no `<tool-use-id>` line ever existed in these blocks.
    fn task_id_only_notification_block(status: &str, summary: &str) -> String {
        format!(
            "<task-notification>\n<task-id>a0343c7095a040965</task-id>\n\
             <output-file>/tmp/tasks/a0343c7095a040965.output</output-file>\n\
             <status>{status}</status>\n<summary>{summary}</summary>\n\
             </task-notification>"
        )
    }

    /// Append the two record shapes a real notification lands as — the
    /// `queue-operation` copy and the injected `user` delivery — to the
    /// parent transcript in the given project alias.
    fn append_parent_notification(
        home: &Path,
        project: &str,
        parent: &str,
        tool_use_id: &str,
        status: &str,
        summary: &str,
        ts: &str,
    ) {
        let block = notification_block(tool_use_id, status, summary);
        append_parent_notification_block(home, project, parent, &block, ts);
    }

    /// Low-level form of [`append_parent_notification`] for tests that
    /// exercise non-default block shapes (re-run toolu, task-id-only
    /// vintage, untagged recaps).
    fn append_parent_notification_block(
        home: &Path,
        project: &str,
        parent: &str,
        block: &str,
        ts: &str,
    ) {
        let path = home
            .join(".claude")
            .join("projects")
            .join(project)
            .join(format!("{parent}.jsonl"));
        let mut body = std::fs::read_to_string(&path).unwrap_or_default();
        body.push_str(
            &serde_json::json!({
                "type": "queue-operation",
                "operation": "enqueue",
                "timestamp": ts,
                "sessionId": parent,
                "content": block,
            })
            .to_string(),
        );
        body.push('\n');
        body.push_str(
            &serde_json::json!({
                "parentUuid": "u-parent",
                "isSidechain": false,
                "type": "user",
                "message": { "role": "user", "content": block },
                "timestamp": ts,
                "sessionId": parent,
            })
            .to_string(),
        );
        body.push('\n');
        std::fs::write(&path, body).unwrap();
    }

    const COMPLETED_ROW: &str =
        "Task complete: Claude Code subagent completed: Agent \"turn alignment\" finished";

    #[test]
    fn completed_notification_serves_synthetic_terminal_row() {
        let home = tempfile::tempdir().unwrap();
        write_subagent(
            home.path(),
            "-repo-project",
            PARENT,
            "a0343c7095a040965",
            TOOL_USE_ID,
            &fixture_lines(),
        );
        append_parent_notification(
            home.path(),
            "-repo-project",
            PARENT,
            TOOL_USE_ID,
            "completed",
            "Agent \"turn alignment\" finished",
            "2026-07-17T10:00:30.000Z",
        );
        let entries = external_session_entries_from_home(home.path(), "claude-code", TASK_ID)
            .expect("task child entries");
        // Exactly ONE terminal row, even though the parent carries the
        // block twice (queue-operation + user delivery).
        let terminals: Vec<_> = entries
            .iter()
            .filter(|entry| {
                entry.get("kind").and_then(|v| v.as_str()) == Some(SUBAGENT_TERMINAL_KIND)
            })
            .collect();
        assert_eq!(terminals.len(), 1);
        let terminal = entries.last().expect("entries end with the terminal row");
        assert_eq!(
            terminal.get("kind").and_then(|v| v.as_str()),
            Some(SUBAGENT_TERMINAL_KIND)
        );
        assert_eq!(
            terminal.get("content").and_then(|v| v.as_str()),
            Some(COMPLETED_ROW)
        );
        assert_eq!(terminal.get("level").and_then(|v| v.as_str()), Some("info"));
        assert_eq!(
            terminal.get("source").and_then(|v| v.as_str()),
            Some("Claude Code")
        );
        // Annotated like every served entry: stable id, delivery, ts.
        assert!(terminal
            .get("event_id")
            .and_then(|v| v.as_str())
            .is_some_and(|id| !id.is_empty()));
        assert_eq!(
            terminal.get("ts").and_then(|v| v.as_str()),
            Some("2026-07-17T10:00:30.000Z")
        );
        assert!(terminal.get("ts_ms").and_then(|v| v.as_i64()).is_some());

        // The detail page (the hydration fetch) serves the same row.
        let body = external_session_detail_from_home_with_page(
            home.path(),
            "claude-code",
            TASK_ID,
            None,
            None,
        )
        .expect("detail body");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        let detail_entries = value.get("entries").and_then(|v| v.as_array()).unwrap();
        let last = detail_entries.last().expect("detail entries");
        assert_eq!(
            last.get("content").and_then(|v| v.as_str()),
            Some(COMPLETED_ROW)
        );
        assert_eq!(
            last.get("kind").and_then(|v| v.as_str()),
            Some(SUBAGENT_TERMINAL_KIND)
        );
    }

    #[test]
    fn non_completed_notifications_add_no_terminal_row() {
        // Errored / stopped children keep today's behavior: no synthetic
        // row (their live path already persists a SessionEnded).
        for status in ["failed", "stopped", "killed"] {
            let home = tempfile::tempdir().unwrap();
            write_subagent(
                home.path(),
                "-repo-project",
                PARENT,
                "a0343c7095a040965",
                TOOL_USE_ID,
                &fixture_lines(),
            );
            append_parent_notification(
                home.path(),
                "-repo-project",
                PARENT,
                TOOL_USE_ID,
                status,
                "Agent \"turn alignment\" ended",
                "2026-07-17T10:00:30.000Z",
            );
            let entries = external_session_entries_from_home(home.path(), "claude-code", TASK_ID)
                .expect("task child entries");
            assert_eq!(entries.len(), 2, "status {status} must not add a row");
            assert_eq!(
                entries[1].get("content").and_then(|v| v.as_str()),
                Some("Starting on the alignment.")
            );
        }
    }

    #[test]
    fn last_notification_wins_across_reruns() {
        // failed then completed (a resumed child finishing cleanly):
        // recency wins, the terminal row appears.
        let home = tempfile::tempdir().unwrap();
        write_subagent(
            home.path(),
            "-repo-project",
            PARENT,
            "a0343c7095a040965",
            TOOL_USE_ID,
            &fixture_lines(),
        );
        append_parent_notification(
            home.path(),
            "-repo-project",
            PARENT,
            TOOL_USE_ID,
            "failed",
            "boom",
            "2026-07-17T10:00:30.000Z",
        );
        append_parent_notification(
            home.path(),
            "-repo-project",
            PARENT,
            TOOL_USE_ID,
            "completed",
            "second run done",
            "2026-07-17T10:05:00.000Z",
        );
        let entries = external_session_entries_from_home(home.path(), "claude-code", TASK_ID)
            .expect("task child entries");
        assert_eq!(
            entries
                .last()
                .and_then(|e| e.get("content"))
                .and_then(|v| v.as_str()),
            Some("Task complete: Claude Code subagent completed: second run done")
        );

        // completed then failed: the stale completion must NOT resurface.
        let home = tempfile::tempdir().unwrap();
        write_subagent(
            home.path(),
            "-repo-project",
            PARENT,
            "a0343c7095a040965",
            TOOL_USE_ID,
            &fixture_lines(),
        );
        append_parent_notification(
            home.path(),
            "-repo-project",
            PARENT,
            TOOL_USE_ID,
            "completed",
            "first run done",
            "2026-07-17T10:00:30.000Z",
        );
        append_parent_notification(
            home.path(),
            "-repo-project",
            PARENT,
            TOOL_USE_ID,
            "failed",
            "rerun boom",
            "2026-07-17T10:05:00.000Z",
        );
        let entries = external_session_entries_from_home(home.path(), "claude-code", TASK_ID)
            .expect("task child entries");
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn rerun_completion_under_new_tool_use_heals_by_task_id() {
        // The relic class: a background sub interrupted and re-run
        // notifies each later run under a NEW tool_use id, so the final
        // completed block never carries the spawn toolu. The `<task-id>`
        // (= agentId = the subagent file stem) is the stable key.
        let home = tempfile::tempdir().unwrap();
        write_subagent(
            home.path(),
            "-repo-project",
            PARENT,
            "a0343c7095a040965",
            TOOL_USE_ID,
            &fixture_lines(),
        );
        let block = rerun_notification_block(
            "toolu_01RERUNRUNRUNRUN9",
            "completed",
            "second run finished clean",
        );
        assert!(
            !block.contains(TOOL_USE_ID),
            "the re-run block must not carry the spawn toolu"
        );
        append_parent_notification_block(
            home.path(),
            "-repo-project",
            PARENT,
            &block,
            "2026-07-17T10:20:00.000Z",
        );
        let entries = external_session_entries_from_home(home.path(), "claude-code", TASK_ID)
            .expect("task child entries");
        let terminal = entries.last().expect("terminal row");
        assert_eq!(
            terminal.get("kind").and_then(|v| v.as_str()),
            Some(SUBAGENT_TERMINAL_KIND)
        );
        assert_eq!(
            terminal.get("content").and_then(|v| v.as_str()),
            Some("Task complete: Claude Code subagent completed: second run finished clean")
        );
    }

    #[test]
    fn task_id_only_vintage_completion_derives_terminal() {
        // Older notification vintages (and earlier-segment spawns, where
        // the spawn toolu is not in this parent segment at all) carry no
        // `<tool-use-id>` line — `<task-id>` alone must resolve.
        let home = tempfile::tempdir().unwrap();
        write_subagent(
            home.path(),
            "-repo-project",
            PARENT,
            "a0343c7095a040965",
            TOOL_USE_ID,
            &fixture_lines(),
        );
        let block = task_id_only_notification_block("completed", "untagged vintage done");
        append_parent_notification_block(
            home.path(),
            "-repo-project",
            PARENT,
            &block,
            "2026-07-17T10:21:00.000Z",
        );
        let entries = external_session_entries_from_home(home.path(), "claude-code", TASK_ID)
            .expect("task child entries");
        assert_eq!(
            entries
                .last()
                .and_then(|e| e.get("content"))
                .and_then(|v| v.as_str()),
            Some("Task complete: Claude Code subagent completed: untagged vintage done")
        );
    }

    #[test]
    fn untagged_multi_task_stop_recap_is_not_terminal_evidence() {
        // A multi-task "stopped recap" block carries NO `<task-id>` /
        // `<tool-use-id>` tags — the ids appear only in prose, which must
        // not match the full-tag needles (and its status would fail the
        // completed gate regardless). No terminal row may derive from it.
        let home = tempfile::tempdir().unwrap();
        write_subagent(
            home.path(),
            "-repo-project",
            PARENT,
            "a0343c7095a040965",
            TOOL_USE_ID,
            &fixture_lines(),
        );
        let recap = format!(
            "<task-notification>\n<status>stopped</status>\n\
             <summary>2 background tasks stopped</summary>\n\
             <note>Agents a0343c7095a040965 ({TOOL_USE_ID}) and zfff0123456789abc \
             were stopped before completion.</note>\n</task-notification>"
        );
        append_parent_notification_block(
            home.path(),
            "-repo-project",
            PARENT,
            &recap,
            "2026-07-17T10:22:00.000Z",
        );
        let entries = external_session_entries_from_home(home.path(), "claude-code", TASK_ID)
            .expect("task child entries");
        assert_eq!(entries.len(), 2);
        assert!(!entries.iter().any(|entry| {
            entry.get("kind").and_then(|v| v.as_str()) == Some(SUBAGENT_TERMINAL_KIND)
        }));
    }

    #[test]
    fn transcript_rows_newer_than_notification_suppress_stale_terminal() {
        // A resumed child streams rows after the old completion — the
        // stale evidence must not paint the live-again child as done.
        let home = tempfile::tempdir().unwrap();
        let mut lines = fixture_lines();
        lines.push(sidechain_line(
            "assistant",
            "a-2",
            "2026-07-17T10:10:00.000Z",
            serde_json::json!([{ "type": "text", "text": "Resumed and working again." }]),
        ));
        write_subagent(
            home.path(),
            "-repo-project",
            PARENT,
            "a0343c7095a040965",
            TOOL_USE_ID,
            &lines,
        );
        append_parent_notification(
            home.path(),
            "-repo-project",
            PARENT,
            TOOL_USE_ID,
            "completed",
            "Agent \"turn alignment\" finished",
            "2026-07-17T10:00:30.000Z",
        );
        let entries = external_session_entries_from_home(home.path(), "claude-code", TASK_ID)
            .expect("task child entries");
        assert!(!entries.iter().any(|entry| {
            entry.get("kind").and_then(|v| v.as_str()) == Some(SUBAGENT_TERMINAL_KIND)
        }));
    }

    #[test]
    fn quoted_notification_in_assistant_row_is_not_evidence() {
        // The parent model quoting the block in an assistant message must
        // not count — only the queue-operation / user record shapes do.
        let home = tempfile::tempdir().unwrap();
        write_subagent(
            home.path(),
            "-repo-project",
            PARENT,
            "a0343c7095a040965",
            TOOL_USE_ID,
            &fixture_lines(),
        );
        let block = notification_block(TOOL_USE_ID, "completed", "quoted");
        let path = home
            .path()
            .join(".claude")
            .join("projects")
            .join("-repo-project")
            .join(format!("{PARENT}.jsonl"));
        std::fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "assistant",
                    "message": { "role": "assistant",
                        "content": [{ "type": "text", "text": block }] },
                    "timestamp": "2026-07-17T10:00:30.000Z",
                    "sessionId": PARENT,
                })
            ),
        )
        .unwrap();
        let entries = external_session_entries_from_home(home.path(), "claude-code", TASK_ID)
            .expect("task child entries");
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn late_notification_upgrades_cached_no_terminal_entries() {
        // The completion lands in the PARENT file after the subagent's
        // last write: a parse cached in that gap must not pin IDLE
        // forever. No-terminal cache hits re-derive; terminal entries
        // are final and re-serve the same Arc.
        let home = tempfile::tempdir().unwrap();
        write_subagent(
            home.path(),
            "-repo-project",
            PARENT,
            "a0343c7095a040965",
            TOOL_USE_ID,
            &fixture_lines(),
        );
        // Parent transcript exists (so the artifacts resolve with it) but
        // carries no notification yet.
        let parent_path = home
            .path()
            .join(".claude")
            .join("projects")
            .join("-repo-project")
            .join(format!("{PARENT}.jsonl"));
        std::fs::write(&parent_path, "").unwrap();

        let first = external_session_entries_from_home_arc(home.path(), "claude-code", TASK_ID)
            .expect("pre-notification entries");
        assert_eq!(first.len(), 2);

        append_parent_notification(
            home.path(),
            "-repo-project",
            PARENT,
            TOOL_USE_ID,
            "completed",
            "Agent \"turn alignment\" finished",
            "2026-07-17T10:00:30.000Z",
        );
        // The subagent transcript is untouched — only the parent grew.
        let second = external_session_entries_from_home_arc(home.path(), "claude-code", TASK_ID)
            .expect("post-notification entries");
        assert_eq!(second.len(), 3);
        assert_eq!(
            second
                .last()
                .and_then(|e| e.get("content"))
                .and_then(|v| v.as_str()),
            Some(COMPLETED_ROW)
        );
        // Terminal entries are final: later parent growth (the thread
        // moving on) neither drops the row nor doubles it, whether the
        // request is served from the cache or re-derived.
        let mut body = std::fs::read_to_string(&parent_path).unwrap();
        body.push_str("{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"parent moved on\"}]},\"timestamp\":\"2026-07-17T11:00:00.000Z\"}\n");
        std::fs::write(&parent_path, body).unwrap();
        let third = external_session_entries_from_home_arc(home.path(), "claude-code", TASK_ID)
            .expect("entries after parent growth");
        let terminals = third
            .iter()
            .filter(|entry| {
                entry.get("kind").and_then(|v| v.as_str()) == Some(SUBAGENT_TERMINAL_KIND)
            })
            .count();
        assert_eq!(terminals, 1);
    }

    /// The dashboard fragment carries the one unavoidable mirror of the
    /// terminal-row contract (the merge-dedupe guard's kind check and the
    /// phase derivation's content prefix); pin both to the Rust source so
    /// a rename here fails the suite instead of shipping as drift.
    #[test]
    fn terminal_row_contract_is_pinned_in_dashboard_fragment() {
        let fragment = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("static/app/39-session-windows.js"),
        )
        .expect("dashboard fragment 39-session-windows.js");
        assert!(
            fragment.contains(&format!("'{SUBAGENT_TERMINAL_KIND}'")),
            "the merge guard must key on kind '{SUBAGENT_TERMINAL_KIND}'"
        );
        assert!(
            fragment.contains("startsWith('Task complete:')"),
            "the restored-phase derivation must key on the 'Task complete:' prefix"
        );
    }

    #[test]
    fn missing_parent_transcript_serves_entries_without_terminal() {
        let home = tempfile::tempdir().unwrap();
        write_subagent(
            home.path(),
            "-repo-project",
            PARENT,
            "a0343c7095a040965",
            TOOL_USE_ID,
            &fixture_lines(),
        );
        let artifacts = find_claude_task_subagent_artifacts(home.path(), TASK_ID)
            .expect("artifacts resolve without a parent transcript");
        assert_eq!(artifacts.tool_use_id, TOOL_USE_ID);
        // The stem's `agent-` prefix is stripped: the field is the raw
        // agentId, exactly what `<task-id>` carries.
        assert_eq!(artifacts.agent_id, "a0343c7095a040965");
        assert!(artifacts.parent_transcript.is_none());
        let entries = external_session_entries_from_home(home.path(), "claude-code", TASK_ID)
            .expect("task child entries");
        assert_eq!(entries.len(), 2);
    }
}
