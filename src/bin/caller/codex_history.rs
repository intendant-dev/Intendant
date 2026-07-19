//! Codex session-history parsing for the controller: locating rollout
//! files under `$CODEX_HOME` and reconstructing per-user-turn revision
//! state from recorded events.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

// Consolidated (message-search F3 phase 2): the rollout-file shapes this
// module used to duplicate live in `external_agent`; the local copies had
// already drifted (whole-file reads vs streaming, a stale injection list).
// `codex_message_content_text` died outright — its one caller now goes
// through the shared `codex_payload_text` shape.
pub(crate) use crate::external_agent::codex::rollout::codex_session_file_id;

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
    // Codex embeds the session id in rollout filenames — try the cheap
    // name match before opening every file (ported from the catalog's
    // finder during consolidation; the id check stays authoritative).
    let found = files
        .iter()
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(session_id))
                && codex_session_file_id(path).as_deref() == Some(session_id)
        })
        .cloned()
        .or_else(|| {
            files
                .into_iter()
                .find(|path| codex_session_file_id(path).as_deref() == Some(session_id))
        })?;
    let _ = crate::external_wrapper_index::record_rollout_path(home, "codex", session_id, &found);
    Some(found)
}

/// A genuine user message from a `response_item` payload: the shared
/// `message` shape, filtered to `role == "user"` and stripped of
/// harness-injected text no human typed.
pub(crate) fn codex_payload_user_text(payload: &serde_json::Value) -> Option<String> {
    let (role, text) = crate::external_agent::codex::rollout::codex_payload_text(payload)?;
    if role != "user" || is_codex_injected_user_text_for_main(&text) {
        return None;
    }
    Some(text)
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
    // Delegates to the canonical predicate at its post-F3 home (this was a
    // byte-for-byte copy that had already drifted once before).
    crate::external_agent::transcript_text::is_injected_external_user_text(text)
}

/// User-turn state for a resumed Codex thread. The transcript's prompt
/// ordinal is the turn authority (see `claude_history.rs` for the full
/// rationale), so this must count EXACTLY the rows the replay/hydration
/// lane (`session_catalog::transcripts::parse_codex_session_entries`)
/// assigns ordinals to — the same steer ledger the catalog builds for
/// this session is applied here.
pub(crate) fn codex_user_turn_state_from_history(
    home: &Path,
    session_id: &str,
) -> Option<UserTurnRevisionState> {
    let path = find_codex_session_file_for_main(home, session_id)?;
    let steers = crate::web_gateway::external_mid_turn_steer_ledger(home, "codex", session_id);
    codex_user_turn_state_from_history_file(&path, &steers)
}

/// Reconstruct per-turn revision state from one rollout file, mirroring
/// the replay lane's admission rule row for row (the parity oracle test
/// below pins the composition):
///
/// - Lane preference mirrors `codex_session_canonical_lanes`: ANY
///   `user_message` event proves the event lane (even one whose row the
///   parser then drops), and provider `response_item` user messages are
///   then ignored entirely — so each candidate lane gets its own steer
///   cursor, exactly like the parser's single cursor only ever seeing one
///   lane's user rows.
/// - A row is counted only when the replay renders AND numbers it
///   (`push_codex_transcript_message` → `push_external_transcript_entry`):
///   text present and non-empty, not harness-injected
///   (`is_injected_external_user_text`), and not a ledger-proven mid-turn
///   steer (those render turnless).
/// - `thread_rolled_back` rewinds recorded turns, so a replaced prompt
///   re-seeds with a bumped revision like the replay's replacement rows.
pub(crate) fn codex_user_turn_state_from_history_file(
    path: &Path,
    steers: &crate::web_gateway::ExternalSteerLedger,
) -> Option<UserTurnRevisionState> {
    let contents = std::fs::read_to_string(path).ok()?;
    let mut saw_user_message_event = false;
    let mut event_state = UserTurnRevisionState::default();
    let mut fallback_state = UserTurnRevisionState::default();
    let mut event_steers = steers.cursor();
    let mut fallback_steers = steers.cursor();

    for line in contents.lines() {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let row_ts_ms = obj
            .get("timestamp")
            .and_then(|v| v.as_str())
            .and_then(crate::web_gateway::timestamp_millis_from_str);
        match obj.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "event_msg" => {
                let Some(payload) = obj.get("payload") else {
                    continue;
                };
                match payload.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "user_message" => {
                        saw_user_message_event = true;
                        let Some(text) =
                            crate::external_agent::codex::rollout::codex_event_message_text(
                                payload,
                            )
                            .map(|(_, text)| text)
                        else {
                            continue;
                        };
                        if text.trim().is_empty() || is_codex_injected_user_text_for_main(&text) {
                            continue;
                        }
                        if event_steers.try_consume_mid_turn_steer(&text, row_ts_ms) {
                            continue;
                        }
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
            "response_item" => {
                let Some(text) = obj.get("payload").and_then(codex_payload_user_text) else {
                    continue;
                };
                if text.trim().is_empty() {
                    continue;
                }
                if fallback_steers.try_consume_mid_turn_steer(&text, row_ts_ms) {
                    continue;
                }
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

        let state = codex_user_turn_state_from_history_file(
            &path,
            &crate::web_gateway::ExternalSteerLedger::default(),
        )
        .expect("state");
        assert_eq!(state.active_count(), 1);
        assert_eq!(state.active_revision(1), Some(1));
        assert_eq!(state.active_revision(2), None);
    }

    /// The resume-seed ⇄ replay-lane parity oracle (Codex twin of
    /// `claude_resume_seed_matches_replay_lane_prompt_ordinals`): over one
    /// fixture rollout containing normal prompts, a provider-request
    /// duplicate, harness-injected user shapes, an empty user_message, a
    /// ledger-proven mid-turn steer, and a thread_rolled_back +
    /// replacement, the seed's active count and revisions equal what
    /// `parse_codex_session_entries` serves as live (non-superseded)
    /// prompt ordinals — so a resumed live lane continues the transcript's
    /// numbering. Exercises the same pipeline the daemon runs at resume:
    /// wrapper-index steer-ledger build → filtered event-lane count.
    #[test]
    fn codex_resume_seed_matches_replay_lane_prompt_ordinals() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "019efeed-aaaa-bbbb-cccc-turnseed0002";
        let wrapper_id = "wrapper-codex-turn-seed";
        let steer_text = "also update the changelog";

        // Wrapper session log carrying the mid-turn steer arc (request +
        // accepted), discovered through the wrapper index like the live
        // catalog's ledger build.
        let wrapper_dir = home.path().join(".intendant").join("logs").join(wrapper_id);
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        let requested_ts_ms = chrono::DateTime::parse_from_rfc3339("2026-05-17T16:49:29Z")
            .unwrap()
            .timestamp_millis();
        std::fs::write(
            wrapper_dir.join("session.jsonl"),
            [
                serde_json::json!({
                    "ts": "16:49:29", "ts_ms": requested_ts_ms,
                    "event": "steer_requested", "level": "info",
                    "message": format!("Steer requested: {steer_text}"),
                    "data": { "session_id": session_id, "id": "steer-1", "status": "pending", "text": steer_text },
                }),
                serde_json::json!({
                    "ts": "16:49:29", "ts_ms": requested_ts_ms + 150,
                    "event": "steer_accepted", "level": "info",
                    "message": "Steer accepted",
                    "data": { "session_id": session_id, "id": "steer-1", "status": "accepted" },
                }),
            ]
            .into_iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            home.path(),
            "codex",
            session_id,
            wrapper_id,
            &wrapper_dir,
            None,
        )
        .unwrap();

        let path = home.path().join("rollout.jsonl");
        let lines = [
            serde_json::json!({
                "timestamp": "2026-05-17T16:48:52Z", "type": "session_meta",
                "payload": { "id": session_id }
            }),
            // Provider-request duplicate of the first prompt: skipped
            // outright when the event lane is canonical.
            serde_json::json!({
                "timestamp": "2026-05-17T16:49:00Z", "type": "response_item",
                "payload": { "type": "message", "role": "user",
                    "content": [{ "type": "input_text", "text": "fix the flaky test" }] }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T16:49:00Z", "type": "event_msg",
                "payload": { "type": "user_message", "message": "fix the flaky test" }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T16:49:05Z", "type": "event_msg",
                "payload": { "type": "agent_message", "message": "On it." }
            }),
            // Harness-injected and empty user events: lane markers only,
            // no row, no ordinal.
            serde_json::json!({
                "timestamp": "2026-05-17T16:49:20Z", "type": "event_msg",
                "payload": { "type": "user_message",
                    "message": "<subagent_notification>\n{\"agent_path\":\"child\"}\n</subagent_notification>" }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T16:49:21Z", "type": "event_msg",
                "payload": { "type": "user_message", "message": "" }
            }),
            // The mid-turn steer, echoed after its wrapper-log request:
            // renders turnless and burns no ordinal.
            serde_json::json!({
                "timestamp": "2026-05-17T16:49:30Z", "type": "event_msg",
                "payload": { "type": "user_message", "message": steer_text }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T16:49:40Z", "type": "event_msg",
                "payload": { "type": "user_message", "message": "second prompt" }
            }),
            // Rollback supersedes "second prompt"; its replacement re-seeds
            // turn 2 at revision 2.
            serde_json::json!({
                "timestamp": "2026-05-17T16:49:50Z", "type": "event_msg",
                "payload": { "type": "thread_rolled_back", "num_turns": 1 }
            }),
            serde_json::json!({
                "timestamp": "2026-05-17T16:49:55Z", "type": "event_msg",
                "payload": { "type": "user_message", "message": "second prompt revised" }
            }),
        ];
        std::fs::write(
            &path,
            lines
                .into_iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let steers =
            crate::web_gateway::external_mid_turn_steer_ledger(home.path(), "codex", session_id);
        let seed = codex_user_turn_state_from_history_file(&path, &steers).expect("seed");
        let entries =
            crate::web_gateway::parse_codex_session_entries(&path, &steers).expect("replay");

        let live_user_rows: Vec<(&str, Option<u64>, Option<u64>)> = entries
            .iter()
            .filter(|e| {
                e["source"] == "user" && e.get("superseded").and_then(|v| v.as_bool()) != Some(true)
            })
            .map(|e| {
                (
                    e["content"].as_str().unwrap_or(""),
                    e.get("user_turn_index").and_then(|v| v.as_u64()),
                    e.get("user_turn_revision").and_then(|v| v.as_u64()),
                )
            })
            .collect();
        assert_eq!(
            live_user_rows,
            vec![
                ("fix the flaky test", Some(1), Some(1)),
                (steer_text, None, None),
                ("second prompt revised", Some(2), Some(2)),
            ],
            "replay lane numbers non-injected, non-steer prompts; the rollback \
             replacement carries a bumped revision"
        );
        let replay_max = live_user_rows
            .iter()
            .filter_map(|(_, turn, _)| *turn)
            .max()
            .unwrap_or(0);
        assert_eq!(
            u64::from(seed.active_count()),
            replay_max,
            "resume seed must equal the replay lane's highest live prompt ordinal"
        );
        assert_eq!(seed.active_count(), 2);
        assert_eq!(
            seed.active_revision(1),
            Some(1),
            "untouched turns keep revision 1"
        );
        assert_eq!(
            seed.active_revision(2),
            Some(2),
            "the rolled-back-and-replaced turn's revision matches the replay's replacement row"
        );

        // The live lane's next prompt continues the transcript numbering.
        let mut live = seed;
        assert_eq!(live.record_next_turn(), (3, 1));
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
