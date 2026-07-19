//! Resume seeding for supervised Claude Code sessions: derive the live
//! loop's `UserTurnRevisionState` from the resumed thread's own
//! transcript.
//!
//! The transcript's prompt ordinal is the turn authority for external
//! sessions: the dashboard's replay/hydration lane
//! (`session_catalog::transcripts::parse_claude_session_entries`) counts
//! non-injected, non-steer user prompts positionally over the WHOLE
//! resumed JSONL, and the reload annotator
//! (`annotate_replay_user_turns_from_external_transcript`) overwrites
//! persisted rows with those ordinals. A resumed live lane that restarts
//! counting at turn 1 therefore disagrees with every hydrated row — the
//! same prompt renders twice under different turn badges, and edits are
//! rejected as "no longer active context" because the daemon validates
//! the transcript-numbered index against its own restarted state.
//!
//! Seeding reuses the catalog's own parser + mid-turn steer ledger via
//! `external_session_entries_from_home_arc` (derive, don't mirror), so
//! the live lane continues at exactly the ordinal the transcript lane
//! will serve for the resumed history.

use std::path::Path;

use crate::codex_history::UserTurnRevisionState;

/// User-turn state for a resumed Claude Code thread, counted from the
/// same parsed transcript snapshot the session catalog serves (warming
/// the catalog's shared cache as a side effect). `None` when no
/// transcript resolves for `session_id` under `home`.
pub(crate) fn claude_user_turn_state_from_history(
    home: &Path,
    session_id: &str,
) -> Option<UserTurnRevisionState> {
    let entries = crate::web_gateway::external_session_entries_from_home_arc(
        home,
        crate::external_agent::AgentBackend::ClaudeCode.as_short_str(),
        session_id,
    )?;
    Some(user_turn_state_from_transcript_entries(entries.iter()))
}

/// Seed a fresh state to the highest `user_turn_index` any transcript
/// entry carries. Max — not row count — so turnless user rows (mid-turn
/// steers) and non-user rows can never skew the seed off the transcript
/// lane's numbering.
pub(crate) fn user_turn_state_from_transcript_entries<'a>(
    entries: impl IntoIterator<Item = &'a serde_json::Value>,
) -> UserTurnRevisionState {
    let max_user_turn = entries
        .into_iter()
        .filter_map(|entry| entry.get("user_turn_index").and_then(|v| v.as_u64()))
        .max()
        .unwrap_or(0)
        .min(u64::from(u32::MAX)) as u32;
    let mut state = UserTurnRevisionState::default();
    state.seed_active_turns_to(max_user_turn);
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_entry_seed_uses_max_turn_index_and_ignores_turnless_rows() {
        let entries = vec![
            serde_json::json!({
                "source": "User", "content": "first prompt",
                "user_turn_index": 1, "user_turn_revision": 1,
            }),
            serde_json::json!({ "source": "Claude Code", "content": "a reply" }),
            serde_json::json!({ "source": "User", "content": "a mid-turn steer" }),
            serde_json::json!({
                "source": "User", "content": "second prompt",
                "user_turn_index": 2, "user_turn_revision": 1,
            }),
        ];
        let state = user_turn_state_from_transcript_entries(entries.iter());
        assert_eq!(state.active_count(), 2);

        let empty = user_turn_state_from_transcript_entries(std::iter::empty::<&serde_json::Value>());
        assert_eq!(empty.active_count(), 0);
    }

    fn rfc3339_ms(value: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(value)
            .expect("fixture timestamp")
            .timestamp_millis()
    }

    /// The resume-seed ⇄ replay-lane parity oracle: over one fixture JSONL
    /// containing normal prompts, harness-injected shapes
    /// (`is_injected_external_user_text`: task notifications, interrupt
    /// markers), tool_result/meta plumbing, and a ledger-proven mid-turn
    /// steer, the seed equals the hydration lane's highest prompt ordinal
    /// — so a resumed live lane's next prompt continues the transcript's
    /// numbering instead of restarting at 1 (the double-row T1-vs-T14
    /// class). Exercises the full pipeline the daemon runs at resume:
    /// transcript discovery → wrapper-index steer-ledger build → parse.
    #[test]
    fn claude_resume_seed_matches_replay_lane_prompt_ordinals() {
        let home = tempfile::tempdir().unwrap();
        let session_id = "0197feed-aaaa-bbbb-cccc-turnseed0001";
        let wrapper_id = "wrapper-cc-turn-seed";
        let steer_text = "also update the changelog";

        // Wrapper session log carrying the mid-turn steer arc (request +
        // accepted), discovered through the wrapper index like the live
        // catalog's ledger build.
        let wrapper_dir = home.path().join(".intendant").join("logs").join(wrapper_id);
        std::fs::create_dir_all(&wrapper_dir).unwrap();
        let requested_ts_ms = rfc3339_ms("2026-07-15T10:00:29Z");
        std::fs::write(
            wrapper_dir.join("session.jsonl"),
            [
                serde_json::json!({
                    "ts": "10:00:29", "ts_ms": requested_ts_ms,
                    "event": "steer_requested", "level": "info",
                    "message": format!("Steer requested: {steer_text}"),
                    "data": { "session_id": session_id, "id": "steer-1", "status": "pending", "text": steer_text },
                }),
                serde_json::json!({
                    "ts": "10:00:29", "ts_ms": requested_ts_ms + 150,
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
            "claude-code",
            session_id,
            wrapper_id,
            &wrapper_dir,
            None,
        )
        .unwrap();

        // The resumed thread's own transcript, under the store layout the
        // catalog's fast path resolves (`projects/<project>/<id>.jsonl`).
        let project_dir = home
            .path()
            .join(".claude")
            .join("projects")
            .join("-tmp-turn-seed-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let lines = [
            r#"{"type":"user","timestamp":"2026-07-15T10:00:00.000Z","message":{"role":"user","content":"fix the flaky test"}}"#.to_string(),
            r#"{"type":"assistant","timestamp":"2026-07-15T10:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"On it."}]}}"#.to_string(),
            // Harness-injected shapes: no row, no index.
            r#"{"type":"user","timestamp":"2026-07-15T10:00:20.000Z","message":{"role":"user","content":"<task-notification>build finished</task-notification>"}}"#.to_string(),
            // The mid-turn steer, echoed after its wrapper-log request:
            // renders turnless and burns no index.
            format!(
                r#"{{"type":"user","timestamp":"2026-07-15T10:00:30.000Z","message":{{"role":"user","content":"{steer_text}"}}}}"#
            ),
            r#"{"type":"user","timestamp":"2026-07-15T10:00:40.000Z","message":{"role":"user","content":"[Request interrupted by user]"}}"#.to_string(),
            r#"{"type":"user","timestamp":"2026-07-15T10:00:50.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"ok"}]}}"#.to_string(),
            r#"{"type":"user","isMeta":true,"timestamp":"2026-07-15T10:00:55.000Z","message":{"role":"user","content":"Caveat: harness-generated"}}"#.to_string(),
            // Block-form follow-up (image rides along): one turn.
            r#"{"type":"user","timestamp":"2026-07-15T10:01:00.000Z","message":{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGk="}},{"type":"text","text":"now fix the docs"}]}}"#.to_string(),
            r#"{"type":"user","timestamp":"2026-07-15T10:02:00.000Z","message":{"role":"user","content":"ship it"}}"#.to_string(),
        ];
        std::fs::write(
            project_dir.join(format!("{session_id}.jsonl")),
            lines.join("\n"),
        )
        .unwrap();

        let seed = claude_user_turn_state_from_history(home.path(), session_id)
            .expect("resumed transcript should seed");
        let entries = crate::web_gateway::external_session_entries_from_home(
            home.path(),
            "claude-code",
            session_id,
        )
        .expect("replay lane should parse the same transcript");

        let user_rows: Vec<(&str, Option<u64>, Option<u64>)> = entries
            .iter()
            .filter(|e| e["source"] == "User")
            .map(|e| {
                (
                    e["content"].as_str().unwrap_or(""),
                    e.get("user_turn_index").and_then(|v| v.as_u64()),
                    e.get("user_turn_revision").and_then(|v| v.as_u64()),
                )
            })
            .collect();
        assert_eq!(
            user_rows,
            vec![
                ("fix the flaky test", Some(1), Some(1)),
                (steer_text, None, None),
                ("now fix the docs", Some(2), Some(1)),
                ("ship it", Some(3), Some(1)),
            ],
            "replay lane counts prompt ordinals; injected/steer rows burn no index"
        );
        let replay_max = user_rows
            .iter()
            .filter_map(|(_, turn, _)| *turn)
            .max()
            .unwrap_or(0);
        assert_eq!(
            u64::from(seed.active_count()),
            replay_max,
            "resume seed must equal the replay lane's highest prompt ordinal"
        );
        assert_eq!(seed.active_count(), 3);

        // The live lane's next prompt continues the transcript numbering.
        let mut live = seed;
        assert_eq!(live.record_next_turn(), (4, 1));
    }
}
