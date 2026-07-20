//! Kimi Code session → message-search extraction.
//!
//! A native session is one `state.json` plus `agents/*/wire.jsonl`. Main and
//! child prose are published under the canonical parent session id, with child
//! records tagged `subagent`; each record id starts with its agent id, so
//! same-named wire events across agents cannot collide. `context.undo` remains
//! searchable through exact record-id supersession marks.

use super::cursor::SourceCursor;
use super::record::{cap_text, Locator, MessageRecord, Role, Source, SupersessionMark};
use super::store::SessionShard;
use crate::web_gateway::session_catalog::kimi_history::{parse_kimi_session, KimiSessionLocation};

pub(crate) fn extract_kimi_session(
    location: KimiSessionLocation,
) -> std::io::Result<(SessionShard, Vec<SourceCursor>)> {
    let session_id = location.session_id.clone();
    // Capture state before parsing any wire. If state or a wire changes
    // during extraction, the saved cursor describes the bytes actually
    // observed and the next sweep classifies the newer tail/rewrite.
    let state_len = std::fs::metadata(&location.state_path)?.len();
    let state_cursor = SourceCursor::capture(&location.state_path, state_len).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "Kimi state vanished during extraction: {}",
                location.state_path.display()
            ),
        )
    })?;
    let parsed = parse_kimi_session(location.clone());
    let mut shard = SessionShard::default();
    let mut superseded_ids = Vec::new();
    let mut superseded_at = 0i64;

    for agent in &parsed.agents {
        for entry in &agent.entries {
            let role = match entry.get("role").and_then(|value| value.as_str()) {
                Some("user")
                    if !entry
                        .get("system_trigger")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false)
                        || agent.subagent =>
                {
                    Role::User
                }
                Some("assistant")
                    if entry.get("kind").and_then(|value| value.as_str()) != Some("reasoning") =>
                {
                    Role::Assistant
                }
                _ => continue,
            };
            let Some(text) = entry
                .get("content")
                .and_then(|value| value.as_str())
                .filter(|text| !text.trim().is_empty())
                .map(str::to_string)
            else {
                continue;
            };
            let Some(record_id) = entry
                .get("record_id")
                .and_then(|value| value.as_str())
                .filter(|id| !id.trim().is_empty())
                .map(str::to_string)
            else {
                continue;
            };
            let ts_ms = entry
                .get("ts")
                .and_then(|value| value.as_str())
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.timestamp_millis())
                .unwrap_or(0);
            let (text, truncated) = cap_text(text);
            if entry
                .get("superseded")
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
            {
                superseded_ids.push(record_id.clone());
                superseded_at = superseded_at.max(
                    entry
                        .get("superseded_at")
                        .and_then(|value| value.as_str())
                        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                        .map(|value| value.timestamp_millis())
                        .unwrap_or(ts_ms),
                );
            }
            shard.records.push(MessageRecord {
                source: Source::Kimi,
                session_id: session_id.clone(),
                role,
                ts_ms,
                text,
                locator: Locator::ExternalRecordId {
                    record_id: record_id.clone(),
                },
                seq: None,
                user_turn: None,
                item_id: Some(record_id),
                subagent: agent.subagent,
                generation: 0,
                truncated,
            });
        }
    }
    if !superseded_ids.is_empty() {
        superseded_ids.sort();
        superseded_ids.dedup();
        shard.marks.push(SupersessionMark::RecordIds {
            record_ids: superseded_ids,
            at_ms: superseded_at,
            reason: "context_undo".to_string(),
        });
    }

    let mut cursors = vec![state_cursor];
    for agent in &location.agents {
        let consumed = parsed
            .agents
            .iter()
            .find(|history| history.agent_id == agent.id)
            .map(|history| history.consumed_bytes)
            .unwrap_or(0);
        let cursor = SourceCursor::capture(&agent.wire_path, consumed).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "Kimi wire vanished during extraction: {}",
                    agent.wire_path.display()
                ),
            )
        })?;
        cursors.push(cursor);
    }
    Ok((shard, cursors))
}

#[cfg(test)]
mod tests {
    use super::super::record::derive_active;
    use super::*;
    use std::path::Path;

    const SESSION: &str = "session_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    fn fixture(home: &Path) -> KimiSessionLocation {
        let dir = home.join(".kimi-code/sessions/wd_repo").join(SESSION);
        std::fs::create_dir_all(dir.join("agents/main")).unwrap();
        std::fs::create_dir_all(dir.join("agents/agent-0")).unwrap();
        std::fs::write(
            dir.join("state.json"),
            serde_json::json!({
                "createdAt":"2026-07-19T10:00:00.000Z",
                "updatedAt":"2026-07-19T10:01:00.000Z",
                "workDir":"/repo",
                "agents":{
                    "main":{"type":"main","parentAgentId":null},
                    "agent-0":{"type":"sub","parentAgentId":"main"}
                }
            })
            .to_string(),
        )
        .unwrap();
        let main = [
            serde_json::json!({"type":"turn.prompt","input":[{"type":"text","text":"old searchable prompt"}],"origin":{"kind":"user"},"time":1784455200000i64}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"content.part","uuid":"old-a","part":{"type":"text","text":"old searchable answer"}},"time":1784455200100i64}),
            serde_json::json!({"type":"context.undo","count":1,"time":1784455200200i64}),
            serde_json::json!({"type":"turn.prompt","input":[{"type":"text","text":"new searchable prompt"}],"origin":{"kind":"user"},"time":1784455200300i64}),
        ];
        std::fs::write(
            dir.join("agents/main/wire.jsonl"),
            main.iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("agents/agent-0/wire.jsonl"),
            serde_json::json!({"type":"turn.prompt","input":[{"type":"text","text":"child searchable prompt"}],"origin":{"kind":"system_trigger"},"time":1784455200150i64}).to_string() + "\n",
        )
        .unwrap();
        crate::web_gateway::session_catalog::kimi_history::find_kimi_session_in(
            &home.join(".kimi-code"),
            SESSION,
        )
        .unwrap()
    }

    #[test]
    fn extracts_main_and_child_and_preserves_undo_as_supersession() {
        let home = tempfile::tempdir().unwrap();
        let location = fixture(home.path());
        let main_wire = location
            .agents
            .iter()
            .find(|agent| agent.id == "main")
            .unwrap()
            .wire_path
            .clone();
        {
            use std::io::Write;
            let mut wire = std::fs::OpenOptions::new()
                .append(true)
                .open(&main_wire)
                .unwrap();
            write!(wire, "{{\"type\":\"turn.prompt\"").unwrap();
        }
        let (shard, cursors) = extract_kimi_session(location).unwrap();
        assert_eq!(cursors.len(), 3);
        assert_eq!(
            cursors
                .iter()
                .find(|cursor| cursor.path == main_wire)
                .unwrap()
                .check(),
            super::super::cursor::CursorCheck::Appended,
            "a torn live tail must remain beyond the published cursor"
        );
        assert_eq!(shard.records.len(), 4);
        assert_eq!(
            shard
                .records
                .iter()
                .filter(|record| record.subagent)
                .count(),
            1
        );
        assert!(shard
            .records
            .iter()
            .all(|record| record.source == Source::Kimi));
        let active = derive_active(&shard.records, &shard.marks);
        let by_text = shard
            .records
            .iter()
            .zip(active)
            .map(|(record, active)| (record.text.as_str(), active))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(by_text["old searchable prompt"], false);
        assert_eq!(by_text["old searchable answer"], false);
        assert_eq!(by_text["new searchable prompt"], true);
        assert_eq!(by_text["child searchable prompt"], true);
    }
}
