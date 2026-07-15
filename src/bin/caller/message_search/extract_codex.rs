//! Codex rollout → message-search extraction (B3 of the message-search
//! program; plan `~/message-search-plan.md` §5). Walks ONE rollout file
//! into a [`SessionShard`]: canonical user/assistant prose plus the
//! supersession marks the reader replays (`derive_active`) — the
//! extractor never computes superseded status itself.
//!
//! Lane rules, grounded in the S0a corpus measurement (2026-07-11):
//! - User text is dual-represented: an `event_msg`/`user_message` twins
//!   a `response_item` with role `user` 99.3% of the time, full-text
//!   exact — counting both would double the user lane. The `event_msg`
//!   lane is canonical here; `response_item` user items are skipped
//!   entirely, which also removes the machine injections that ride that
//!   lane with no event twin (AGENTS.md dumps, `<environment_context>`,
//!   `<turn_aborted>`, … — 39% of naive user-lane bytes). The injection
//!   filter still guards the event lane as cheap insurance.
//! - Assistant prose is canonical on `response_item` `message` items
//!   with role `assistant`; `developer`-role items are harness config
//!   and are skipped. (`event_msg`/`agent_message` is the assistant
//!   twin lane — unused, matching the catalog's canonical-items rule.)
//! - `thread_rolled_back` events append in place: `num_turns` and/or an
//!   item anchor (anchors usually arrive with `num_turns == 0`).
//!
//! Hermetic: the caller hands the rollout path (plus the previously
//! published shard and a generation number on rewrite detection);
//! nothing here resolves homes or the environment.

use super::cursor::{for_each_complete_line_from, SourceCursor};
use super::record::{cap_text, Locator, MessageRecord, Role, Source, SupersessionMark};
use super::store::SessionShard;
use crate::external_agent::codex::rollout::{
    codex_event_message_text, codex_payload_text, codex_response_item_id,
    codex_thread_rollback_anchor, is_codex_injected_user_text, value_str,
};
use std::path::Path;

/// Extract one Codex rollout file into a session shard + source cursors.
///
/// `generation` tags every record parsed from the CURRENT file bytes.
/// `prior` is the session's previously published shard; pass it — with a
/// bumped, strictly higher `generation` — when the source was detected
/// REWRITTEN (a same-path cursor returning `CursorCheck::Rewritten`: a
/// Codex same-thread restore rewrites the rollout in place). The prior
/// generations' records are then retained and republished alongside the
/// new generation's, never discarded (plan §5 index-everything), with a
/// [`SupersessionMark::GenerationRestore`] recording that the freshly
/// parsed generation is the active branch. On the normal (append/new)
/// path pass `prior: None`: the full re-parse IS the session's shard.
///
/// Prior `TurnCount`/`ItemAnchor` marks are deliberately NOT republished
/// across a generation merge: they were scoped to the abandoned branch's
/// record frame, and replayed over the merged record vec they alias onto
/// the new generation's records (turn ordinals restart per parse; anchor
/// item ids recur in restored prefixes), which would make LIVE messages
/// read superseded. Dropping them fails in the safe direction — an
/// abandoned branch over-reads as active (D2 surfaces superseded hits by
/// default anyway) rather than the live thread under-reading. The prior
/// `GenerationRestore` chain (branch transitions) is kept.
pub(crate) fn extract_codex_session(
    rollout_path: &Path,
    prior: Option<&SessionShard>,
    generation: u32,
) -> std::io::Result<(SessionShard, Vec<SourceCursor>)> {
    let mut session_id: Option<String> = None;
    let mut records: Vec<MessageRecord> = Vec::new();
    let mut marks: Vec<SupersessionMark> = Vec::new();
    // Codex user-turn ordinal frame: increments on EVERY
    // `event_msg`/`user_message` (mirroring the wrapper-side
    // `UserTurnRevisionState` counting), including messages the filters
    // below drop, so record ordinals stay aligned with the frame
    // `thread_rolled_back.num_turns` counts in.
    let mut user_turn: u32 = 0;
    // Every rollout line carries an RFC3339 `timestamp`; carry the last
    // parseable one forward for (rare) lines missing their own.
    let mut ts_ms: i64 = 0;

    let mut line_no: u64 = 0;
    let consumed = for_each_complete_line_from(rollout_path, 0, |line| {
        line_no += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            return;
        };
        if let Some(parsed) = value_str(&obj, "timestamp").as_deref().and_then(rfc3339_ms) {
            ts_ms = parsed;
        }
        let Some(payload) = obj.get("payload") else {
            return;
        };
        match obj.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "session_meta" => {
                // `payload.id` is this rollout's own id (the rule
                // `codex_session_file_id` applies); the sibling
                // `session_id` field is the SPAWNING thread on subagent
                // rollouts — not this file's identity.
                if session_id.is_none() {
                    session_id = value_str(payload, "id");
                }
            }
            "event_msg" => match payload.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "thread_rolled_back" => {
                    let num_turns = payload
                        .get("num_turns")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if num_turns > 0 {
                        marks.push(SupersessionMark::TurnCount {
                            // Untrusted on-disk count: clamp for the cast;
                            // replay is bounded by the turns that exist.
                            num_turns: num_turns.min(u32::MAX as u64) as u32,
                            at_ms: ts_ms,
                        });
                    }
                    if let Some((item_id, position)) = codex_thread_rollback_anchor(payload) {
                        marks.push(SupersessionMark::ItemAnchor {
                            item_id,
                            position,
                            at_ms: ts_ms,
                        });
                    }
                }
                "user_message" => {
                    user_turn = user_turn.saturating_add(1);
                    let Some((_, text)) = codex_event_message_text(payload) else {
                        return;
                    };
                    if text.trim().is_empty() || is_codex_injected_user_text(&text) {
                        return;
                    }
                    records.push(message_record(
                        Role::User,
                        text,
                        ts_ms,
                        generation,
                        line_no,
                        None,
                        Some(user_turn),
                    ));
                }
                _ => {}
            },
            "response_item" => {
                let Some((role, text)) = codex_payload_text(payload) else {
                    return;
                };
                // role "user" is the event_msg twin lane (dedup) plus the
                // machine injections; "developer" is harness config.
                if role != "assistant" || text.trim().is_empty() {
                    return;
                }
                let item_id = codex_response_item_id(&obj);
                // Assistant records carry the ordinal of the user turn
                // they follow: `TurnCount` supersession is defined as the
                // rolled-back user turns AND their following assistant
                // records (record.rs), and the replay keys on `user_turn`.
                let turn = (user_turn > 0).then_some(user_turn);
                records.push(message_record(
                    Role::Assistant,
                    text,
                    ts_ms,
                    generation,
                    line_no,
                    item_id,
                    turn,
                ));
            }
            _ => {}
        }
    })?;

    let session_id = session_id.unwrap_or_else(|| {
        // Degenerate rollout with no session_meta line: fall back to the
        // (unique) file stem so records still key deterministically.
        rollout_path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    for record in &mut records {
        record.session_id = session_id.clone();
    }

    let shard = match prior {
        None => SessionShard { records, marks },
        Some(prior) => {
            let mut merged_records = prior.records.clone();
            merged_records.extend(records);
            let mut merged_marks: Vec<SupersessionMark> = prior
                .marks
                .iter()
                .filter(|mark| matches!(mark, SupersessionMark::GenerationRestore { .. }))
                .cloned()
                .collect();
            merged_marks.push(SupersessionMark::GenerationRestore {
                active_generation: generation,
                at_ms: ts_ms,
            });
            merged_marks.extend(marks);
            SessionShard {
                records: merged_records,
                marks: merged_marks,
            }
        }
    };

    let cursors = SourceCursor::capture(rollout_path, consumed)
        .into_iter()
        .collect();
    Ok((shard, cursors))
}

fn rfc3339_ms(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

fn message_record(
    role: Role,
    text: String,
    ts_ms: i64,
    generation: u32,
    line_no: u64,
    item_id: Option<String>,
    user_turn: Option<u32>,
) -> MessageRecord {
    // Fingerprint the FULL extracted text (pre-cap): the hash is the
    // message's content identity, not the truncation's.
    let content_hash16 = crate::session_log::content_hash_hex16(&text);
    let (text, truncated) = cap_text(text);
    let locator = match item_id.as_deref() {
        Some(record_id) => Locator::ExternalRecordId {
            record_id: record_id.to_string(),
        },
        None => Locator::ExternalLine {
            generation,
            line_no,
            content_hash16,
        },
    };
    MessageRecord {
        source: Source::Codex,
        session_id: String::new(), // backfilled once session_meta is known
        role,
        ts_ms,
        text,
        locator,
        seq: None,
        user_turn,
        item_id,
        subagent: false,
        generation,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::super::cursor::CursorCheck;
    use super::super::record::derive_active;
    use super::*;
    use std::io::Write;

    fn ms(ts: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(ts)
            .unwrap()
            .timestamp_millis()
    }

    fn write_rollout(path: &Path, lines: &[serde_json::Value]) {
        let mut file = std::fs::File::create(path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    fn meta(id: &str) -> serde_json::Value {
        serde_json::json!({
            "timestamp": "2026-07-01T10:00:00.000Z",
            "type": "session_meta",
            // `session_id` is the spawning thread on subagent rollouts —
            // a decoy the extractor must NOT adopt.
            "payload": { "id": id, "session_id": "parent-thread-decoy" }
        })
    }

    fn user_event(ts: &str, text: &str) -> serde_json::Value {
        serde_json::json!({
            "timestamp": ts,
            "type": "event_msg",
            "payload": {
                "type": "user_message", "message": text,
                "images": [], "local_images": [], "text_elements": []
            }
        })
    }

    fn message_item(ts: &str, role: &str, id: Option<&str>, text: &str) -> serde_json::Value {
        let block_type = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        let mut payload = serde_json::json!({
            "type": "message", "role": role,
            "content": [ { "type": block_type, "text": text } ]
        });
        if let Some(id) = id {
            payload["id"] = serde_json::json!(id);
        }
        serde_json::json!({ "timestamp": ts, "type": "response_item", "payload": payload })
    }

    fn rollback(ts: &str, num_turns: u64, anchor: Option<(&str, &str)>) -> serde_json::Value {
        let mut payload =
            serde_json::json!({ "type": "thread_rolled_back", "num_turns": num_turns });
        if let Some((item_id, position)) = anchor {
            payload["anchor"] = serde_json::json!({ "itemId": item_id, "position": position });
        }
        serde_json::json!({ "timestamp": ts, "type": "event_msg", "payload": payload })
    }

    #[test]
    fn user_lane_dedups_response_item_twins() {
        // S0a: 99.3% of event_msg user messages have an exact full-text
        // response_item twin in the same file — the event lane alone is
        // canonical, or the user lane double-counts.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_rollout(
            &path,
            &[
                meta("sess-1"),
                user_event("2026-07-01T10:00:01.000Z", "find the bug"),
                message_item("2026-07-01T10:00:01.500Z", "user", None, "find the bug"),
                message_item(
                    "2026-07-01T10:00:02.000Z",
                    "assistant",
                    Some("msg_a1"),
                    "looking now",
                ),
            ],
        );
        let (shard, cursors) = extract_codex_session(&path, None, 0).unwrap();

        assert_eq!(
            shard.records.len(),
            2,
            "twin skipped: one user, one assistant"
        );
        let user = &shard.records[0];
        assert_eq!(user.role, Role::User);
        assert_eq!(user.source, Source::Codex);
        assert_eq!(
            user.session_id, "sess-1",
            "payload.id, not the session_id decoy"
        );
        assert_eq!(user.text, "find the bug");
        assert_eq!(user.user_turn, Some(1));
        assert_eq!(user.ts_ms, ms("2026-07-01T10:00:01.000Z"));
        assert!(
            matches!(
                user.locator,
                Locator::ExternalLine {
                    generation: 0,
                    line_no: 2,
                    ..
                }
            ),
            "event user records have no native id: line locator ({:?})",
            user.locator
        );

        let assistant = &shard.records[1];
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.text, "looking now");
        assert_eq!(assistant.item_id.as_deref(), Some("msg_a1"));
        assert_eq!(assistant.user_turn, Some(1), "follows user turn 1");
        assert!(matches!(
            &assistant.locator,
            Locator::ExternalRecordId { record_id } if record_id == "msg_a1"
        ));

        assert!(shard.marks.is_empty());
        assert_eq!(cursors.len(), 1);
        assert_eq!(
            cursors[0].last_complete_line_offset,
            std::fs::metadata(&path).unwrap().len(),
            "whole file consumed"
        );
    }

    #[test]
    fn injected_user_text_and_developer_items_are_skipped() {
        // S0a: response_item user items without an event twin are ~all
        // machine injections (39% of naive user-lane bytes); the twin-lane
        // skip removes them by construction, and the injection filter
        // also guards the event lane. Developer-role items are config.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_rollout(
            &path,
            &[
                meta("sess-2"),
                user_event(
                    "2026-07-01T11:00:00.000Z",
                    "<environment_context>cwd=/tmp</environment_context>",
                ),
                message_item(
                    "2026-07-01T11:00:00.100Z",
                    "user",
                    None,
                    "# AGENTS.md instructions for repo\nnever index me",
                ),
                message_item(
                    "2026-07-01T11:00:00.200Z",
                    "developer",
                    None,
                    "<permissions instructions>never index me either",
                ),
                user_event("2026-07-01T11:00:01.000Z", "real question"),
            ],
        );
        let (shard, _) = extract_codex_session(&path, None, 0).unwrap();

        assert_eq!(shard.records.len(), 1);
        assert_eq!(shard.records[0].text, "real question");
        // The injected user_message still advanced the turn frame (Codex
        // counts every user_message event in num_turns).
        assert_eq!(shard.records[0].user_turn, Some(2));
    }

    #[test]
    fn turn_count_rollback_supersedes_the_rolled_turns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let rollback_ts = "2026-07-01T12:00:05.000Z";
        write_rollout(
            &path,
            &[
                meta("sess-3"),
                user_event("2026-07-01T12:00:01.000Z", "first ask"),
                message_item(
                    "2026-07-01T12:00:02.000Z",
                    "assistant",
                    Some("msg_1"),
                    "first answer",
                ),
                user_event("2026-07-01T12:00:03.000Z", "second ask"),
                message_item(
                    "2026-07-01T12:00:04.000Z",
                    "assistant",
                    Some("msg_2"),
                    "second answer",
                ),
                rollback(rollback_ts, 1, None),
            ],
        );
        let (shard, _) = extract_codex_session(&path, None, 0).unwrap();

        assert_eq!(
            shard.marks,
            vec![SupersessionMark::TurnCount {
                num_turns: 1,
                at_ms: ms(rollback_ts),
            }]
        );
        // The reader derives: turn 2 (its user message AND its following
        // assistant record) superseded, turn 1 untouched.
        let active = derive_active(&shard.records, &shard.marks);
        assert_eq!(active, vec![true, true, false, false]);
    }

    #[test]
    fn item_anchor_rollback_with_zero_turns() {
        // Anchored rewinds usually arrive with num_turns == 0: no
        // TurnCount mark, only the anchor.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        let rollback_ts = "2026-07-01T13:00:05.000Z";
        write_rollout(
            &path,
            &[
                meta("sess-4"),
                user_event("2026-07-01T13:00:01.000Z", "ask one"),
                message_item(
                    "2026-07-01T13:00:02.000Z",
                    "assistant",
                    Some("msg_1"),
                    "answer one",
                ),
                user_event("2026-07-01T13:00:03.000Z", "ask two"),
                message_item(
                    "2026-07-01T13:00:04.000Z",
                    "assistant",
                    Some("msg_2"),
                    "answer two",
                ),
                rollback(rollback_ts, 0, Some(("msg_2", "before"))),
            ],
        );
        let (shard, _) = extract_codex_session(&path, None, 0).unwrap();

        assert_eq!(
            shard.marks,
            vec![SupersessionMark::ItemAnchor {
                item_id: "msg_2".into(),
                position: "before".into(),
                at_ms: ms(rollback_ts),
            }]
        );
        let active = derive_active(&shard.records, &shard.marks);
        assert_eq!(
            active,
            vec![true, true, true, false],
            "everything from the anchored item on is superseded"
        );
    }

    #[test]
    fn rewrite_retains_prior_generation_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_rollout(
            &path,
            &[
                meta("sess-5"),
                user_event("2026-07-01T14:00:01.000Z", "alpha"),
                message_item(
                    "2026-07-01T14:00:02.000Z",
                    "assistant",
                    Some("msg_1"),
                    "alpha answer",
                ),
                user_event("2026-07-01T14:00:03.000Z", "beta"),
                rollback("2026-07-01T14:00:04.000Z", 1, None),
            ],
        );
        let (gen0, cursors0) = extract_codex_session(&path, None, 0).unwrap();
        assert_eq!(gen0.records.len(), 3);
        assert_eq!(gen0.marks.len(), 1);

        // Same-thread restore rewrites the rollout in place: the restored
        // prefix (original timestamps and item ids) plus a new branch.
        write_rollout(
            &path,
            &[
                meta("sess-5"),
                user_event("2026-07-01T14:00:01.000Z", "alpha"),
                message_item(
                    "2026-07-01T14:00:02.000Z",
                    "assistant",
                    Some("msg_1"),
                    "alpha answer",
                ),
                user_event("2026-07-01T14:00:05.000Z", "gamma"),
            ],
        );
        assert_eq!(
            cursors0[0].check(),
            CursorCheck::Rewritten,
            "the caller's rewrite signal — this is what bumps the generation"
        );

        let (merged, _) = extract_codex_session(&path, Some(&gen0), 1).unwrap();

        // Prior generation retained in full (index-everything: the "beta"
        // branch stays findable), new parse rides alongside as gen 1.
        assert_eq!(merged.records.len(), 3 + 3);
        assert_eq!(merged.records[..3], gen0.records[..]);
        assert!(merged.records[3..].iter().all(|r| r.generation == 1));
        let texts: Vec<&str> = merged.records.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "alpha",
                "alpha answer",
                "beta",
                "alpha",
                "alpha answer",
                "gamma"
            ]
        );
        assert!(
            matches!(
                merged.records[3].locator,
                Locator::ExternalLine {
                    generation: 1,
                    line_no: 2,
                    ..
                }
            ),
            "line locators pin the generation that produced them"
        );

        // Marks: the prior branch's TurnCount is not republished (it
        // would alias onto the new generation's restarted turn ordinals);
        // the merge records the branch transition instead.
        assert_eq!(
            merged.marks,
            vec![SupersessionMark::GenerationRestore {
                active_generation: 1,
                at_ms: ms("2026-07-01T14:00:05.000Z"),
            }]
        );
        // Derived status: everything stays findable-active — the frozen
        // reader treats retained branches as live unless marks say
        // otherwise; D2 badges rather than hides superseded hits anyway.
        let active = derive_active(&merged.records, &merged.marks);
        assert!(active.iter().all(|&a| a));
    }

    #[test]
    fn partial_trailing_line_waits_for_completion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_rollout(
            &path,
            &[
                meta("sess-6"),
                user_event("2026-07-01T15:00:01.000Z", "committed line"),
            ],
        );
        let complete_len = std::fs::metadata(&path).unwrap().len();
        let tail = user_event("2026-07-01T15:00:02.000Z", "still being written").to_string();
        let (head, rest) = tail.split_at(tail.len() / 2);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(head.as_bytes()).unwrap();
        drop(file);

        let (shard, cursors) = extract_codex_session(&path, None, 0).unwrap();
        assert_eq!(shard.records.len(), 1, "partial trailing line not consumed");
        assert_eq!(shard.records[0].text, "committed line");
        assert_eq!(cursors[0].last_complete_line_offset, complete_len);

        // Completing the line makes it visible to the next pass.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(rest.as_bytes()).unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);
        let (shard, cursors) = extract_codex_session(&path, None, 0).unwrap();
        assert_eq!(shard.records.len(), 2);
        assert_eq!(shard.records[1].text, "still being written");
        assert_eq!(
            cursors[0].last_complete_line_offset,
            std::fs::metadata(&path).unwrap().len()
        );
    }

    #[test]
    fn cursor_round_trip_detects_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        write_rollout(
            &path,
            &[
                meta("sess-7"),
                user_event("2026-07-01T16:00:01.000Z", "one"),
            ],
        );
        let (_, cursors) = extract_codex_session(&path, None, 0).unwrap();
        assert_eq!(cursors[0].check(), CursorCheck::Unchanged);

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", user_event("2026-07-01T16:00:02.000Z", "two")).unwrap();
        drop(file);
        assert_eq!(cursors[0].check(), CursorCheck::Appended);

        let (shard, cursors) = extract_codex_session(&path, None, 0).unwrap();
        assert_eq!(shard.records.len(), 2);
        assert_eq!(cursors[0].check(), CursorCheck::Unchanged);
    }
}
