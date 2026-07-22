//! Pi v3 session tree → message-search extraction.
//!
//! Pi keeps abandoned sibling branches in the same append-only file. All
//! billed user/assistant prose remains searchable, while a `RecordIds` mark
//! identifies rows outside the current last-leaf parent chain so the normal
//! `include_superseded` switch controls whether those branches appear.

use super::cursor::SourceCursor;
use super::record::{cap_text, Locator, MessageRecord, Role, Source, SupersessionMark};
use super::store::SessionShard;
use crate::web_gateway::session_catalog::pi_history::{
    parse_pi_session, pi_entry_timestamp, pi_message_text, PiSessionLocation,
};
use serde_json::Value;
use std::collections::HashSet;

pub(crate) fn extract_pi_session(
    location: PiSessionLocation,
) -> std::io::Result<(SessionShard, Vec<SourceCursor>)> {
    let session_id = location.session_id.clone();
    let history = parse_pi_session(location.clone());
    let cursor =
        SourceCursor::capture(&location.path, history.consumed_bytes).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "Pi session vanished during extraction: {}",
                    location.path.display()
                ),
            )
        })?;
    let active_ids = history
        .active_entries()
        .into_iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .collect::<HashSet<_>>();
    let mut shard = SessionShard::default();
    let mut superseded = Vec::new();
    let mut latest_ts_ms = 0i64;

    for entry in &history.entries {
        if entry.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(record_id) = entry
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let Some(message) = entry.get("message") else {
            continue;
        };
        let role = match message.get("role").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let text = pi_message_text(message);
        if text.trim().is_empty() {
            continue;
        }
        let ts_ms = pi_entry_timestamp(entry)
            .as_deref()
            .and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(timestamp).ok())
            .map(|timestamp| timestamp.timestamp_millis())
            .unwrap_or(0);
        latest_ts_ms = latest_ts_ms.max(ts_ms);
        let (text, truncated) = cap_text(text);
        shard.records.push(MessageRecord {
            source: Source::Pi,
            session_id: session_id.clone(),
            role,
            ts_ms,
            text,
            locator: Locator::ExternalRecordId {
                record_id: record_id.to_string(),
            },
            seq: None,
            user_turn: None,
            item_id: Some(record_id.to_string()),
            subagent: false,
            generation: 0,
            truncated,
        });
        if !active_ids.contains(record_id) {
            superseded.push(record_id.to_string());
        }
    }
    if !superseded.is_empty() {
        superseded.sort();
        superseded.dedup();
        shard.marks.push(SupersessionMark::RecordIds {
            record_ids: superseded,
            at_ms: latest_ts_ms,
            reason: "pi_inactive_branch".to_string(),
        });
    }
    Ok((shard, vec![cursor]))
}

#[cfg(test)]
mod tests {
    use super::super::record::derive_active;
    use super::*;
    use std::path::Path;

    fn fixture(home: &Path) -> PiSessionLocation {
        let dir = home.join(".pi/agent/sessions/--repo--");
        std::fs::create_dir_all(&dir).unwrap();
        let id = "01J_PI_SEARCH";
        let path = dir.join(format!("2026-07-21T00-00-00Z_{id}.jsonl"));
        let values = [
            serde_json::json!({"type":"session","version":3,"id":id,"timestamp":"2026-07-21T00:00:00Z","cwd":"/repo"}),
            serde_json::json!({"type":"message","id":"u1","parentId":null,"timestamp":"2026-07-21T00:00:01Z","message":{"role":"user","content":"active pi prompt","timestamp":1784592001000i64}}),
            serde_json::json!({"type":"message","id":"old","parentId":"u1","timestamp":"2026-07-21T00:00:02Z","message":{"role":"assistant","provider":"openai-codex","model":"gpt","content":[{"type":"text","text":"abandoned pi answer"}],"usage":{},"stopReason":"stop","timestamp":1784592002000i64}}),
            serde_json::json!({"type":"message","id":"new","parentId":"u1","timestamp":"2026-07-21T00:00:03Z","message":{"role":"assistant","provider":"openai-codex","model":"gpt","content":[{"type":"text","text":"current pi answer"}],"usage":{},"stopReason":"stop","timestamp":1784592003000i64}}),
        ];
        std::fs::write(
            &path,
            values
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        crate::web_gateway::session_catalog::pi_history::find_pi_session_in(
            &home.join(".pi/agent"),
            id,
        )
        .unwrap()
    }

    #[test]
    fn indexes_all_billed_branches_and_marks_inactive_sibling() {
        let home = tempfile::tempdir().unwrap();
        let location = fixture(home.path());
        let path = location.path.clone();
        let (shard, cursors) = extract_pi_session(location).unwrap();
        assert_eq!(cursors.len(), 1);
        assert_eq!(cursors[0].path, path);
        assert_eq!(shard.records.len(), 3);
        assert!(shard
            .records
            .iter()
            .all(|record| record.source == Source::Pi));
        let active = derive_active(&shard.records, &shard.marks);
        let states = shard
            .records
            .iter()
            .zip(active)
            .map(|(record, active)| (record.text.as_str(), active))
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            vec![
                ("active pi prompt", true),
                ("abandoned pi answer", false),
                ("current pi answer", true),
            ]
        );
    }
}
