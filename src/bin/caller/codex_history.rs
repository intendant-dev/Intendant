//! Codex session-history parsing for the controller: locating rollout
//! files under `$CODEX_HOME` and reconstructing per-user-turn revision
//! state from recorded events.

use crate::json_string_field;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub(crate) fn collect_jsonl_files(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, out);
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|name| name.ends_with(".jsonl"))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
}

pub(crate) fn codex_session_file_id(path: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if obj.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
            return obj
                .get("payload")
                .and_then(|payload| json_string_field(payload, "id"));
        }
    }
    None
}

pub(crate) fn find_codex_session_file_for_main(home: &Path, session_id: &str) -> Option<PathBuf> {
    let codex = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".codex"));
    find_codex_session_file_in(&codex, home, session_id)
}

/// Locate the rollout whose `session_meta.payload.id` is `session_id`
/// under `codex_root`, consulting the wrapper index's stored rollout path
/// before paying for the recursive scan (the scan opens EVERY rollout
/// under `sessions/` + `archived_sessions/` to match the id). The stored
/// path is trusted only after re-verifying the session id — rollouts
/// migrate `sessions/` → `archived_sessions/` — and a successful rescan
/// re-records the fresh location. `codex_root` is resolved by the caller
/// (the `CODEX_HOME` env edge above); `home` scopes the wrapper index.
pub(crate) fn find_codex_session_file_in(
    codex_root: &Path,
    home: &Path,
    session_id: &str,
) -> Option<PathBuf> {
    if let Some(stored) =
        crate::external_wrapper_index::resolved_rollout_path(home, "codex", session_id, |path| {
            codex_session_file_id(path).as_deref() == Some(session_id)
        })
    {
        return Some(stored);
    }
    let mut files = Vec::new();
    collect_jsonl_files(&codex_root.join("sessions"), &mut files);
    collect_jsonl_files(&codex_root.join("archived_sessions"), &mut files);
    let found = files
        .into_iter()
        .find(|path| codex_session_file_id(path).as_deref() == Some(session_id))?;
    let _ = crate::external_wrapper_index::record_rollout_path(home, "codex", session_id, &found);
    Some(found)
}

pub(crate) fn codex_message_content_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|item| {
                    item.get("text")
                        .and_then(|v| v.as_str())
                        .or_else(|| item.get("content").and_then(|v| v.as_str()))
                        .map(str::to_string)
                })
                .collect();
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

pub(crate) fn codex_payload_user_text(payload: &serde_json::Value) -> Option<String> {
    if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
        return None;
    }
    if payload.get("role").and_then(|v| v.as_str()) != Some("user") {
        return None;
    }
    let text = codex_message_content_text(payload.get("content")?)?;
    if is_codex_injected_user_text_for_main(&text) {
        None
    } else {
        Some(text)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct UserTurnRevisionState {
    active_count: u32,
    latest_revision_by_turn: HashMap<u32, u32>,
    active_revision_by_turn: HashMap<u32, u32>,
}

impl UserTurnRevisionState {
    pub(crate) fn active_count(&self) -> u32 {
        self.active_count
    }

    pub(crate) fn active_revision(&self, user_turn_index: u32) -> Option<u32> {
        self.active_revision_by_turn.get(&user_turn_index).copied()
    }

    pub(crate) fn seed_active_turns_to(&mut self, active_count: u32) {
        while self.active_count < active_count {
            self.record_next_turn();
        }
    }

    pub(crate) fn record_next_turn(&mut self) -> (u32, u32) {
        let user_turn_index = self.active_count.saturating_add(1);
        let revision = self.record_active_turn(user_turn_index);
        self.active_count = user_turn_index;
        (user_turn_index, revision)
    }

    pub(crate) fn record_active_turn(&mut self, user_turn_index: u32) -> u32 {
        let next_revision = self
            .latest_revision_by_turn
            .get(&user_turn_index)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        self.latest_revision_by_turn
            .insert(user_turn_index, next_revision);
        self.active_revision_by_turn
            .insert(user_turn_index, next_revision);
        self.active_count = self.active_count.max(user_turn_index);
        next_revision
    }

    pub(crate) fn rewind_last_turns(&mut self, turns_to_drop: u32) {
        if turns_to_drop == 0 || self.active_count == 0 {
            return;
        }
        let first_user_turn_index = self
            .active_count
            .saturating_sub(turns_to_drop)
            .saturating_add(1);
        self.rewind_from_turn(first_user_turn_index);
    }

    pub(crate) fn rewind_from_turn(&mut self, first_user_turn_index: u32) {
        if first_user_turn_index == 0 || first_user_turn_index > self.active_count {
            return;
        }
        for turn in first_user_turn_index..=self.active_count {
            self.active_revision_by_turn.remove(&turn);
        }
        self.active_count = first_user_turn_index.saturating_sub(1);
    }

    pub(crate) fn validate_expected_revision(
        &self,
        user_turn_index: u32,
        expected_revision: Option<u32>,
    ) -> Result<(), String> {
        let Some(expected_revision) = expected_revision else {
            return Err(format!(
                "Cannot edit user turn {}; missing active-message revision",
                user_turn_index
            ));
        };
        match self.active_revision(user_turn_index) {
            Some(active_revision) if active_revision == expected_revision => Ok(()),
            Some(active_revision) => Err(format!(
                "Cannot edit user turn {}; the displayed message revision {} is stale (active revision is {})",
                user_turn_index, expected_revision, active_revision
            )),
            None => Err(format!(
                "Cannot edit user turn {}; that message is no longer active context",
                user_turn_index
            )),
        }
    }
}

pub(crate) fn is_codex_injected_user_text_for_main(text: &str) -> bool {
    // Delegates to the canonical predicate — this was a byte-for-byte copy
    // that had already drifted from the session-catalog vocabulary.
    crate::web_gateway::is_injected_external_user_text(text)
}

pub(crate) fn codex_user_turn_state_from_history(
    session_id: &str,
) -> Option<UserTurnRevisionState> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()));
    let path = find_codex_session_file_for_main(&home, session_id)?;
    codex_user_turn_state_from_history_file(&path)
}

pub(crate) fn codex_user_turn_state_from_history_file(
    path: &Path,
) -> Option<UserTurnRevisionState> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut saw_user_message_event = false;
    let mut event_state = UserTurnRevisionState::default();
    let mut fallback_state = UserTurnRevisionState::default();

    for line in contents.lines() {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match obj.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "event_msg" => {
                let Some(payload) = obj.get("payload") else {
                    continue;
                };
                match payload.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "user_message" => {
                        saw_user_message_event = true;
                        event_state.record_next_turn();
                    }
                    "thread_rolled_back" => {
                        let turns = payload
                            .get("num_turns")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as u32;
                        event_state.rewind_last_turns(turns);
                        fallback_state.rewind_last_turns(turns);
                    }
                    _ => {}
                }
            }
            "response_item"
                if obj
                    .get("payload")
                    .and_then(codex_payload_user_text)
                    .is_some() =>
            {
                fallback_state.record_next_turn();
            }
            _ => {}
        }
    }

    Some(if saw_user_message_event {
        event_state
    } else {
        fallback_state
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_turn_revision_state_rejects_stale_replacement() {
        let mut state = UserTurnRevisionState::default();
        let (turn, revision) = state.record_next_turn();
        assert_eq!((turn, revision), (1, 1));
        assert!(state.validate_expected_revision(1, Some(1)).is_ok());

        state.rewind_from_turn(1);
        let (replacement_turn, replacement_revision) = state.record_next_turn();
        assert_eq!((replacement_turn, replacement_revision), (1, 2));

        let stale = state.validate_expected_revision(1, Some(1)).unwrap_err();
        assert!(stale.contains("stale"), "got: {stale}");
        assert!(state.validate_expected_revision(1, Some(2)).is_ok());
    }

    #[test]
    fn codex_user_turn_state_prefers_user_message_events_over_provider_items() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let session_id = "019e37b2-main-event-user-canonical";
        std::fs::write(
            &path,
            [
                serde_json::json!({
                    "timestamp": "2026-05-17T16:48:52Z",
                    "type": "session_meta",
                    "payload": { "id": session_id }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:00Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "provider request context 1" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:01Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "provider request context 2" }]
                    }
                }),
                serde_json::json!({
                    "timestamp": "2026-05-17T16:49:02Z",
                    "type": "event_msg",
                    "payload": { "type": "user_message", "message": "human prompt" }
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();

        let state = codex_user_turn_state_from_history_file(&path).expect("state");
        assert_eq!(state.active_count(), 1);
        assert_eq!(state.active_revision(1), Some(1));
        assert_eq!(state.active_revision(2), None);
    }

    #[test]
    fn codex_injected_user_text_filters_subagent_notifications() {
        assert!(is_codex_injected_user_text_for_main(
            "<subagent_notification>\n{\"agent_path\":\"child\"}\n</subagent_notification>"
        ));
        assert!(is_codex_injected_user_text_for_main(
            "<environment_context>\n  <cwd>/repo</cwd>\n</environment_context>"
        ));
        assert!(is_codex_injected_user_text_for_main(
            "<user_shell_command>\n<command>\nhtop\n</command>\n</user_shell_command>"
        ));
        assert!(is_codex_injected_user_text_for_main(
            "<task-notification>\n<task-id>child</task-id>\n</task-notification>"
        ));
        assert!(is_codex_injected_user_text_for_main(
            "<command-name>/context</command-name>\n<command-args></command-args>"
        ));
        assert!(is_codex_injected_user_text_for_main(
            "<local-command-stdout>context usage</local-command-stdout>"
        ));
        assert!(is_codex_injected_user_text_for_main(
            "<bash-input>cargo test</bash-input>"
        ));
        assert!(is_codex_injected_user_text_for_main(
            "<bash-stdout>ok</bash-stdout>"
        ));
        assert!(is_codex_injected_user_text_for_main(
            "<bash-stderr>warning</bash-stderr>"
        ));
        assert!(!is_codex_injected_user_text_for_main(
            "please inspect subagent_notification handling"
        ));
        assert!(!is_codex_injected_user_text_for_main(
            "please inspect <bash-input> handling"
        ));
    }

    #[test]
    fn find_codex_session_file_in_reuses_stored_rollout_path_and_survives_migration() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "019ea8b9-0000-7000-8000-00000000dd01";
        let wrapper_id = "9a411507-4a41-4a37-95a2-6a8f4f9edc01";
        let log_dir = home.path().join(".intendant").join("logs").join(wrapper_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        crate::external_wrapper_index::upsert(
            home.path(),
            "codex",
            session_id,
            wrapper_id,
            &log_dir,
            None,
        )
        .unwrap();

        let codex_root = home.path().join("codex-root");
        let sessions_dir = codex_root.join("sessions").join("2026").join("07");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let meta = serde_json::json!({
            "timestamp": "2026-07-11T10:00:00Z",
            "type": "session_meta",
            "payload": { "id": session_id }
        });
        let decoy = serde_json::json!({
            "timestamp": "2026-07-11T10:00:00Z",
            "type": "session_meta",
            "payload": { "id": "019ea8b9-9999-7000-8000-00000000dd99" }
        });
        std::fs::write(sessions_dir.join("decoy.jsonl"), decoy.to_string()).unwrap();
        let live = sessions_dir.join("rollout.jsonl");
        std::fs::write(&live, meta.to_string()).unwrap();

        // First resolution scans and records the location.
        assert_eq!(
            find_codex_session_file_in(&codex_root, home.path(), session_id),
            Some(live.clone())
        );
        assert_eq!(
            crate::external_wrapper_index::resolved_rollout_path(
                home.path(),
                "codex",
                session_id,
                |_| true
            ),
            Some(live.clone())
        );

        // Migration sessions/ -> archived_sessions/: the stored path goes
        // stale, the scan fallback finds the new home and re-records it.
        let archived_dir = codex_root.join("archived_sessions");
        std::fs::create_dir_all(&archived_dir).unwrap();
        let archived = archived_dir.join("rollout.jsonl");
        std::fs::rename(&live, &archived).unwrap();
        assert_eq!(
            find_codex_session_file_in(&codex_root, home.path(), session_id),
            Some(archived.clone())
        );
        assert_eq!(
            crate::external_wrapper_index::resolved_rollout_path(
                home.path(),
                "codex",
                session_id,
                |_| true
            ),
            Some(archived.clone())
        );

        // The stored path short-circuits the scan: a recorded location
        // outside the scanned roots still resolves, as long as the file
        // exists and its session id verifies.
        let outside = codex_root.join("kept.jsonl");
        std::fs::rename(&archived, &outside).unwrap();
        crate::external_wrapper_index::record_rollout_path(
            home.path(),
            "codex",
            session_id,
            &outside,
        )
        .unwrap();
        assert_eq!(
            find_codex_session_file_in(&codex_root, home.path(), session_id),
            Some(outside)
        );
    }
}
