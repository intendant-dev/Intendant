//! Claude Code transcript extractor (message-search B4; plan §5, spike
//! report S0b): one main-session `<uuid>.jsonl` plus its
//! `<uuid>/subagents/agent-*.jsonl` siblings in, one [`SessionShard`] +
//! per-file [`SourceCursor`]s out.
//!
//! Hermetic by construction: every input is a parameter — no env or home
//! resolution here. The wiring edge (a later PR) enumerates
//! `~/.claude/projects/**`, resolves subagent dirs by session uuid
//! CORPUS-WIDE (a session's subdir can live under a DIFFERENT project dir
//! than its main jsonl after a worktree relocation — S0b Q1), and
//! publishes the shard under the session key `claude-code:<session_id>`.
//!
//! Claude Code has no rollback/rewind mechanism, and compaction is not
//! supersession (a compacted message was still said — plan §4/§5), so
//! this extractor NEVER emits `SupersessionMark`s. Transcripts are
//! append-only (S0b Q4), so every record is `generation` 0: a `Rewritten`
//! cursor check means a full re-extract, never a generation bump — record
//! identity is the duplicate-free record `uuid`.

use super::cursor::{for_each_complete_line_from, CursorCheck, SourceCursor};
use super::record::{cap_text, Locator, MessageRecord, Role, Source};
use super::store::SessionShard;
use crate::external_agent::transcript_text::{is_injected_external_user_text, message_prose_text};
use std::path::{Path, PathBuf};

/// Extract one Claude Code session — the main `<uuid>.jsonl` transcript
/// plus its `subagents/agent-*.jsonl` siblings — into a publishable
/// [`SessionShard`] and one [`SourceCursor`] per consumed file.
///
/// * `session_id` — the session uuid (== the main file's stem == the
///   subagent dir's name). Every record, subagent records included, is
///   keyed to it: subagents are indexed under the PARENT session, tagged
///   `subagent` (plan §5; their `sessionId` field equals the parent uuid).
///   The caller publishes the shard as `claude-code:<session_id>`.
/// * `main_path` must be a strict-`.jsonl` transcript; backup artifacts
///   (`*.jsonl.backup`, `*.jsonl.bak-*` — S0b Q6) are refused as
///   `InvalidInput`.
/// * `subagent_paths` entries that are not `agent-*.jsonl` transcripts
///   (meta.json sidecars, backups, workflow journals) are silently
///   skipped, so one sloppy enumeration can neither poison nor fail the
///   session.
///
/// Records are ordered main file first, then subagent files sorted by
/// path, line order within each file: line order is the authoritative
/// intra-file order (S0b Q4 — uuids are random, timestamps tie at ms),
/// there is no authoritative cross-file interleave, and the deterministic
/// walk keeps shard bytes stable so the store's content-named generation
/// files dedup across passes. A partial trailing line (live writer
/// mid-append — S0b gotcha 7) stays unconsumed; its bytes sit past the
/// returned cursor offset and read as `CursorCheck::Appended`.
pub(crate) fn extract_claude_session(
    session_id: &str,
    main_path: &Path,
    subagent_paths: &[PathBuf],
) -> std::io::Result<(SessionShard, Vec<SourceCursor>)> {
    if !is_strict_jsonl(main_path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "not a .jsonl transcript (backup/sidecar artifacts are excluded): {}",
                main_path.display()
            ),
        ));
    }
    let mut shard = SessionShard::default(); // marks stay empty: no Claude supersession
    let mut cursors = Vec::new();
    walk_transcript(
        main_path,
        session_id,
        false,
        &mut shard.records,
        &mut cursors,
    )?;
    for path in claude_transcript_agents(subagent_paths) {
        walk_transcript(path, session_id, true, &mut shard.records, &mut cursors)?;
    }
    Ok((shard, cursors))
}

/// The subagent paths [`extract_claude_session`] actually consumes, in its
/// consumption order (transcript-shaped only, sorted, deduped). The
/// incremental fold matches saved cursors against exactly this set, and
/// only files in this set can ever change the shard.
pub(crate) fn claude_transcript_agents(subagent_paths: &[PathBuf]) -> Vec<&PathBuf> {
    let mut agents: Vec<&PathBuf> = subagent_paths
        .iter()
        .filter(|path| is_subagent_transcript(path))
        .collect();
    agents.sort();
    agents.dedup();
    agents
}

/// Consume every complete line of one transcript file, appending its
/// message-lane records and its cursor.
fn walk_transcript(
    path: &Path,
    session_id: &str,
    subagent: bool,
    records: &mut Vec<MessageRecord>,
    cursors: &mut Vec<SourceCursor>,
) -> std::io::Result<()> {
    let consumed = for_each_complete_line_from(path, 0, |line| {
        if let Some(record) = record_from_line(line, session_id, subagent) {
            records.push(record);
        }
    })?;
    let cursor = SourceCursor::capture(path, consumed).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("transcript vanished during extraction: {}", path.display()),
        )
    })?;
    cursors.push(cursor);
    Ok(())
}

/// Fold a MAIN-transcript append into the session's previously published
/// shard, without re-reading the whole corpus. Sound only in the narrow
/// case the caller verified with cursors: the main file APPENDED (same
/// identity/prefix), every subagent transcript is byte-unchanged, and the
/// subagent set itself did not change. Claude records are line-local
/// ([`record_from_line`] holds no cross-line state), and full extraction
/// orders records [main lines..., agents by path...], so splicing the
/// suffix records at the end of the prior MAIN block reproduces the full
/// re-extraction byte-for-byte — the store's content-named shard files
/// keep deduping across passes.
///
/// Returns `Ok(None)` when the prior shard does not have the expected
/// main-then-agents partition (never produced by this extractor, but a
/// foreign store must fall back to the full parse, never mis-splice), or
/// when the post-fold revalidation shows the source moved under us.
pub(crate) fn fold_claude_main_append(
    session_id: &str,
    main_path: &Path,
    saved: &SourceCursor,
    prior: &SessionShard,
) -> std::io::Result<Option<(SessionShard, SourceCursor)>> {
    if !is_strict_jsonl(main_path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "not a .jsonl transcript (backup/sidecar artifacts are excluded): {}",
                main_path.display()
            ),
        ));
    }
    // The prior records must be one main block followed by one agents
    // block for the splice below to reproduce full-extraction order.
    let main_block_end = prior
        .records
        .iter()
        .rposition(|record| !record.subagent)
        .map(|index| index + 1)
        .unwrap_or(0);
    if prior.records[..main_block_end]
        .iter()
        .any(|record| record.subagent)
    {
        return Ok(None);
    }
    let mut suffix_records = Vec::new();
    let consumed =
        for_each_complete_line_from(main_path, saved.last_complete_line_offset, |line| {
            if let Some(record) = record_from_line(line, session_id, false) {
                suffix_records.push(record);
            }
        })?;
    let Some(cursor) = SourceCursor::capture(main_path, consumed) else {
        return Ok(None);
    };
    // TOCTOU guard: the caller validated the SAVED cursor's windows, then
    // we read the suffix — a rewrite landing in between would splice new
    // bytes onto stale prior records, and the freshly captured cursor
    // above (hashed from the rewritten file) would legitimize the corrupt
    // shard permanently. Re-running the saved cursor's own check AFTER
    // the read costs two ≤4 KiB window reads and closes the laundering:
    // anything but a still-plain append discards the fold. (A rewrite
    // racing the capture itself leaves a cursor that mismatches the file,
    // so the next sweep full-rebuilds — either way, no corrupt steady
    // state.)
    if !matches!(
        saved.check(),
        CursorCheck::Appended | CursorCheck::Unchanged
    ) {
        return Ok(None);
    }
    let mut records = Vec::with_capacity(prior.records.len().saturating_add(suffix_records.len()));
    records.extend_from_slice(&prior.records[..main_block_end]);
    records.append(&mut suffix_records);
    records.extend_from_slice(&prior.records[main_block_end..]);
    Ok(Some((
        SessionShard {
            records,
            marks: prior.marks.clone(),
        },
        cursor,
    )))
}

/// Parse one transcript line into a [`MessageRecord`], or `None` for
/// everything that is not indexable prose (S0b record taxonomy):
///
/// - non-message-lane `type`s: sidecar state rows (`mode`, `ai-title`,
///   `last-prompt`, `file-history-snapshot`, `queue-operation`,
///   `worktree-state`, …), `attachment` machine text, `system` records
///   (incl. `compact_boundary` — compaction is synthetic AND is not
///   supersession, so it yields neither record nor mark), and the extinct
///   1.x `summary` type (tolerated for foreign corpora);
/// - `isMeta: true` synthetic turns and `isCompactSummary` context
///   summaries (machine-generated, not user prose);
/// - records without `uuid` or `timestamp` — sidecar lane by definition;
/// - content with no prose `text` blocks: [`message_prose_text`]
///   structurally rejects tool_use/tool_result blocks, so the parent's
///   copy of a subagent's final report (a `tool_result`) is never
///   double-indexed alongside the agent file's own records (S0b Q5);
/// - harness-injected user-position envelopes
///   ([`is_injected_external_user_text`]: `<task-notification>` async
///   subagent completions, `<command-name>`/`<local-command-*>` slash
///   plumbing, …) — plan §10 keeps system injections out of the index.
fn record_from_line(line: &str, session_id: &str, subagent: bool) -> Option<MessageRecord> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let role = match value.get("type")?.as_str()? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    if value.get("isMeta").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    if value.get("isCompactSummary").and_then(|v| v.as_bool()) == Some(true) {
        return None;
    }
    // Message-lane records always carry both (S0b Q4); anything without
    // them is sidecar-shaped no matter what its `type` claims.
    let record_id = value.get("uuid")?.as_str()?.to_string();
    let ts_raw = value.get("timestamp")?.as_str()?;
    let ts_ms = chrono::DateTime::parse_from_rfc3339(ts_raw)
        .ok()?
        .timestamp_millis();
    let text = message_prose_text(value.get("message")?.get("content")?)?;
    if text.trim().is_empty() {
        return None;
    }
    if role == Role::User && is_injected_external_user_text(&text) {
        return None;
    }
    let (text, truncated) = cap_text(text);
    Some(MessageRecord {
        source: Source::ClaudeCode,
        session_id: session_id.to_string(),
        role,
        ts_ms,
        text,
        locator: Locator::ExternalRecordId { record_id },
        seq: None,
        user_turn: None,
        item_id: None,
        subagent,
        generation: 0,
        truncated,
    })
}

/// Strict `.jsonl` suffix: the corpus carries `*.jsonl.backup`,
/// `*.jsonl.bak-*`, `*.patch`, and atomic-write `*.tmp.*` leftovers next
/// to real transcripts (S0b Q6) — none of them are transcripts.
fn is_strict_jsonl(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
}

/// Subagent transcripts are exactly `agent-<agentId>.jsonl` (S0b Q1). The
/// name gate also keeps `agent-*.meta.json` sidecars and
/// `workflows/**/journal.jsonl` (non-transcript-shaped records) out even
/// when the caller's enumeration was sloppy.
fn is_subagent_transcript(path: &Path) -> bool {
    is_strict_jsonl(path)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("agent-"))
}

#[cfg(test)]
mod tests {
    use super::super::cursor::{read_complete_lines_from, CursorCheck};
    use super::super::record::MESSAGE_TEXT_CAP_BYTES;
    use super::super::store::{PublishOutcome, Store};
    use super::*;

    const SESSION: &str = "128ce827-c24f-42b8-8111-bec913f4098f";

    fn ts_ms(iso: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .timestamp_millis()
    }

    /// A main-transcript message-lane record (S0b Q3 shape).
    fn message_line(
        record_type: &str,
        uuid: &str,
        iso_ts: &str,
        content: serde_json::Value,
        extra: &[(&str, serde_json::Value)],
    ) -> String {
        let mut value = serde_json::json!({
            "parentUuid": null,
            "isSidechain": false,
            "userType": "external",
            "type": record_type,
            "message": { "role": record_type, "content": content },
            "uuid": uuid,
            "timestamp": iso_ts,
            "sessionId": SESSION,
            "version": "2.1.207",
            "gitBranch": "main",
        });
        for (key, extra_value) in extra {
            value[*key] = extra_value.clone();
        }
        value.to_string()
    }

    /// A subagent-file record: same schema plus the three sidechain facts
    /// on every record (S0b Q1) — `sessionId` is the PARENT uuid.
    fn sidechain_line(
        record_type: &str,
        uuid: &str,
        iso_ts: &str,
        text: &str,
        agent_id: &str,
    ) -> String {
        serde_json::json!({
            "parentUuid": null,
            "isSidechain": true,
            "promptId": "acd4cf52-8707-48b5-b5e9-c5a4c7e782f8",
            "agentId": agent_id,
            "type": record_type,
            "message": { "role": record_type, "content": text },
            "uuid": uuid,
            "timestamp": iso_ts,
            "userType": "external",
            "sessionId": SESSION,
            "version": "2.1.207",
        })
        .to_string()
    }

    fn write_transcript(path: &Path, lines: &[String]) {
        let mut body = lines.join("\n");
        body.push('\n');
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn extracts_string_and_array_prose_and_rejects_tool_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join(format!("{SESSION}.jsonl"));
        write_transcript(
            &main,
            &[
                // String content (both eras keep the string-or-array
                // duality — S0b Q6).
                message_line(
                    "user",
                    "u-1",
                    "2026-07-10T10:00:00.000Z",
                    serde_json::json!("find the cursor bug"),
                    &[],
                ),
                // Assistant prose next to a tool_use block: the text block
                // is indexed, the tool_use is structurally rejected.
                message_line(
                    "assistant",
                    "a-1",
                    "2026-07-10T10:00:05.250Z",
                    serde_json::json!([
                        {"type": "text", "text": "Looking at the cursor now."},
                        {"type": "tool_use", "id": "toolu_01", "name": "Read",
                         "input": {"file_path": "/tmp/x"}},
                    ]),
                    &[],
                ),
                // Tool-use-only assistant record: no prose at all.
                message_line(
                    "assistant",
                    "a-2",
                    "2026-07-10T10:00:06.000Z",
                    serde_json::json!([
                        {"type": "tool_use", "id": "toolu_02", "name": "Agent",
                         "input": {"prompt": "go"}},
                    ]),
                    &[],
                ),
                // The parent's copy of a subagent report is a tool_result
                // block — rejected structurally, so scanning the agent
                // file too never double-indexes the report (S0b Q5).
                message_line(
                    "user",
                    "u-2",
                    "2026-07-10T10:00:07.000Z",
                    serde_json::json!([
                        {"type": "tool_result", "tool_use_id": "toolu_02",
                         "content": "SUBAGENT REPORT PROSE"},
                    ]),
                    &[],
                ),
                // Extinct 1.x pointer record: tolerated, skipped.
                r#"{"type":"summary","summary":"old pointer","leafUuid":"x"}"#.to_string(),
                // Corrupt line: skipped, never fatal (S0b gotcha 7).
                "{not json".to_string(),
            ],
        );

        let (shard, cursors) = extract_claude_session(SESSION, &main, &[]).unwrap();
        assert!(
            shard.marks.is_empty(),
            "Claude never emits supersession marks"
        );
        assert_eq!(cursors.len(), 1);
        let texts: Vec<&str> = shard.records.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["find the cursor bug", "Looking at the cursor now."]
        );
        let user = &shard.records[0];
        assert_eq!(user.role, Role::User);
        assert_eq!(user.session_id, SESSION);
        assert_eq!(user.ts_ms, ts_ms("2026-07-10T10:00:00.000Z"));
        assert_eq!(
            user.locator,
            Locator::ExternalRecordId {
                record_id: "u-1".into()
            }
        );
        assert!(!user.subagent);
        assert_eq!(user.generation, 0);
        let assistant = &shard.records[1];
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.ts_ms, ts_ms("2026-07-10T10:00:05.250Z"));
        assert!(!shard
            .records
            .iter()
            .any(|r| r.text.contains("SUBAGENT REPORT PROSE")));
    }

    #[test]
    fn meta_compact_and_sidecar_records_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join(format!("{SESSION}.jsonl"));
        write_transcript(
            &main,
            &[
                // Synthetic continuation turn.
                message_line(
                    "user",
                    "u-meta",
                    "2026-07-10T11:00:00.000Z",
                    serde_json::json!("Continue from where you left off."),
                    &[("isMeta", serde_json::json!(true))],
                ),
                // Compaction pair (S0b Q3): synthetic, and NOT
                // supersession — no records, no marks.
                format!(
                    r#"{{"type":"system","subtype":"compact_boundary","parentUuid":null,"logicalParentUuid":"ae30c6f5","content":"Conversation compacted","level":"info","uuid":"sys-1","timestamp":"2026-07-10T11:00:01.000Z","sessionId":"{SESSION}","isSidechain":false}}"#
                ),
                message_line(
                    "user",
                    "u-compact",
                    "2026-07-10T11:00:02.000Z",
                    serde_json::json!(
                        "This session is being continued from a previous conversation..."
                    ),
                    &[
                        ("isCompactSummary", serde_json::json!(true)),
                        ("isVisibleInTranscriptOnly", serde_json::json!(true)),
                    ],
                ),
                // Sidecar lane (S0b Q3): rewritten state rows without
                // uuid/timestamp — excluded by type before those checks.
                format!(r#"{{"type":"mode","mode":"normal","sessionId":"{SESSION}"}}"#),
                format!(
                    r#"{{"type":"ai-title","aiTitle":"Cursor bug hunt","sessionId":"{SESSION}"}}"#
                ),
                format!(
                    r#"{{"type":"last-prompt","lastPrompt":"pong","leafUuid":"x","sessionId":"{SESSION}"}}"#
                ),
                format!(
                    r#"{{"type":"queue-operation","operation":"enqueue","timestamp":"2026-07-10T11:00:03.000Z","sessionId":"{SESSION}","content":"queued prompt"}}"#
                ),
                r#"{"type":"file-history-snapshot","messageId":"m-1","snapshot":{"timestamp":"2026-07-10T11:00:04.000Z"}}"#
                    .to_string(),
                // Attachment records are machine text (task reminders,
                // tool-state deltas) — not message lane.
                format!(
                    r#"{{"type":"attachment","attachment":{{"type":"task_reminder","content":"reminder"}},"uuid":"att-1","timestamp":"2026-07-10T11:00:05.000Z","sessionId":"{SESSION}","isSidechain":false}}"#
                ),
                // The one real prompt in the file.
                message_line(
                    "user",
                    "u-real",
                    "2026-07-10T11:00:06.000Z",
                    serde_json::json!("real question"),
                    &[],
                ),
            ],
        );

        let (shard, _) = extract_claude_session(SESSION, &main, &[]).unwrap();
        assert!(shard.marks.is_empty(), "compaction must not produce marks");
        assert_eq!(shard.records.len(), 1);
        assert_eq!(shard.records[0].text, "real question");
        assert_eq!(
            shard.records[0].locator,
            Locator::ExternalRecordId {
                record_id: "u-real".into()
            }
        );
    }

    #[test]
    fn injected_user_envelopes_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join(format!("{SESSION}.jsonl"));
        write_transcript(
            &main,
            &[
                // Async subagent completion delivered in user position —
                // the report text must be indexed from the agent file
                // only, never from the notification envelope (S0b Q5).
                message_line(
                    "user",
                    "u-notif",
                    "2026-07-10T12:00:00.000Z",
                    serde_json::json!(
                        "<task-notification><task-id>a015316bed2f2d538</task-id>report body</task-notification>"
                    ),
                    &[],
                ),
                // Slash-command plumbing.
                message_line(
                    "user",
                    "u-cmd",
                    "2026-07-10T12:00:01.000Z",
                    serde_json::json!("<command-name>/compact</command-name>"),
                    &[],
                ),
                message_line(
                    "user",
                    "u-out",
                    "2026-07-10T12:00:02.000Z",
                    serde_json::json!("<local-command-stdout>ok</local-command-stdout>"),
                    &[],
                ),
                message_line(
                    "user",
                    "u-real",
                    "2026-07-10T12:00:03.000Z",
                    serde_json::json!("actual human words"),
                    &[],
                ),
            ],
        );

        let (shard, _) = extract_claude_session(SESSION, &main, &[]).unwrap();
        let texts: Vec<&str> = shard.records.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["actual human words"]);
    }

    #[test]
    fn subagent_records_get_parent_session_and_flag() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join(format!("{SESSION}.jsonl"));
        write_transcript(
            &main,
            &[message_line(
                "user",
                "u-1",
                "2026-07-10T13:00:00.000Z",
                serde_json::json!("spawn two agents"),
                &[],
            )],
        );
        let sub_dir = dir.path().join(SESSION).join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        let agent_b = sub_dir.join("agent-bbb.jsonl");
        let agent_a = sub_dir.join("agent-aaa.jsonl");
        write_transcript(
            &agent_b,
            &[
                sidechain_line(
                    "user",
                    "sb-1",
                    "2026-07-10T13:00:02.000Z",
                    "prompt for b",
                    "bbb",
                ),
                sidechain_line(
                    "assistant",
                    "sb-2",
                    "2026-07-10T13:00:03.000Z",
                    "report from b",
                    "bbb",
                ),
            ],
        );
        write_transcript(
            &agent_a,
            &[sidechain_line(
                "user",
                "sa-1",
                "2026-07-10T13:00:01.000Z",
                "prompt for a",
                "aaa",
            )],
        );

        // Caller enumeration order is not trusted: the walk is main file
        // first, then subagent files sorted by path, so shard bytes are
        // deterministic and content-named generations dedup across passes.
        let (shard, cursors) =
            extract_claude_session(SESSION, &main, &[agent_b.clone(), agent_a.clone()]).unwrap();
        assert_eq!(cursors.len(), 3);
        assert_eq!(cursors[0].path, main);
        assert_eq!(cursors[1].path, agent_a);
        assert_eq!(cursors[2].path, agent_b);
        let seen: Vec<(&str, bool)> = shard
            .records
            .iter()
            .map(|r| (r.text.as_str(), r.subagent))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("spawn two agents", false),
                ("prompt for a", true),
                ("prompt for b", true),
                ("report from b", true),
            ]
        );
        assert!(
            shard.records.iter().all(|r| r.session_id == SESSION),
            "subagent records carry the PARENT session id"
        );
        assert!(shard.marks.is_empty());
    }

    #[test]
    fn backup_and_non_transcript_siblings_are_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join(format!("{SESSION}.jsonl"));
        write_transcript(
            &main,
            &[message_line(
                "user",
                "u-1",
                "2026-07-10T14:00:00.000Z",
                serde_json::json!("hello"),
                &[],
            )],
        );
        let sub_dir = dir.path().join(SESSION).join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        // All of these WOULD parse if read — exclusion is by name (strict
        // `.jsonl` suffix + `agent-` prefix, S0b Q6), not content failure.
        let backup = sub_dir.join("agent-aaa.jsonl.backup");
        let bak = sub_dir.join("agent-bbb.jsonl.bak-20260401");
        let journal = sub_dir.join("journal.jsonl");
        for path in [&backup, &bak, &journal] {
            write_transcript(
                path,
                &[sidechain_line(
                    "user",
                    "poison",
                    "2026-07-10T14:00:01.000Z",
                    "MUST NOT BE INDEXED",
                    "aaa",
                )],
            );
        }
        let meta = sub_dir.join("agent-aaa.meta.json");
        std::fs::write(
            &meta,
            r#"{"agentType":"Explore","toolUseId":"toolu_x","spawnDepth":1}"#,
        )
        .unwrap();

        let (shard, cursors) =
            extract_claude_session(SESSION, &main, &[backup, bak, meta, journal]).unwrap();
        assert_eq!(cursors.len(), 1, "only the main transcript was consumed");
        assert_eq!(shard.records.len(), 1);
        assert_eq!(shard.records[0].text, "hello");

        // A backup artifact as the MAIN path is a caller bug: refuse it.
        let err = extract_claude_session(
            SESSION,
            &dir.path().join(format!("{SESSION}.jsonl.backup")),
            &[],
        )
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn cursor_round_trip_with_partial_trailing_line() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join(format!("{SESSION}.jsonl"));
        let first = message_line(
            "user",
            "u-1",
            "2026-07-10T15:00:00.000Z",
            serde_json::json!("first"),
            &[],
        );
        let second = message_line(
            "assistant",
            "a-1",
            "2026-07-10T15:00:01.000Z",
            serde_json::json!([{"type": "text", "text": "second"}]),
            &[],
        );
        let third = message_line(
            "user",
            "u-2",
            "2026-07-10T15:00:02.000Z",
            serde_json::json!("third arrives later"),
            &[],
        );
        let (head, tail) = third.split_at(third.len() / 2);
        // A live session is mid-write: the third line has no newline yet.
        std::fs::write(&main, format!("{first}\n{second}\n{head}")).unwrap();

        let (shard, cursors) = extract_claude_session(SESSION, &main, &[]).unwrap();
        assert_eq!(shard.records.len(), 2, "partial trailing line stays unread");
        let cursor = &cursors[0];
        assert_eq!(
            cursor.last_complete_line_offset,
            (first.len() + second.len() + 2) as u64
        );
        assert_eq!(
            cursor.check(),
            CursorCheck::Appended,
            "partial bytes already sit past the cursor"
        );

        // The writer finishes the line.
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&main)
                .unwrap();
            writeln!(file, "{tail}").unwrap();
        }

        // An incremental read from the saved cursor yields exactly the
        // completed line...
        let (lines, consumed) =
            read_complete_lines_from(&main, cursor.last_complete_line_offset).unwrap();
        assert_eq!(lines, vec![third.clone()]);
        assert_eq!(
            record_from_line(&lines[0], SESSION, false).unwrap().text,
            "third arrives later"
        );
        // ...and a fresh full walk agrees end to end.
        let (shard, cursors) = extract_claude_session(SESSION, &main, &[]).unwrap();
        assert_eq!(shard.records.len(), 3);
        assert_eq!(cursors[0].last_complete_line_offset, consumed);
        assert_eq!(cursors[0].check(), CursorCheck::Unchanged);
    }

    #[test]
    fn oversized_text_is_capped_and_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join(format!("{SESSION}.jsonl"));
        let blob = "a".repeat(MESSAGE_TEXT_CAP_BYTES + 1024);
        write_transcript(
            &main,
            &[message_line(
                "user",
                "u-big",
                "2026-07-10T17:00:00.000Z",
                serde_json::json!(blob),
                &[],
            )],
        );

        let (shard, _) = extract_claude_session(SESSION, &main, &[]).unwrap();
        assert_eq!(shard.records.len(), 1);
        assert!(shard.records[0].truncated);
        assert_eq!(shard.records[0].text.len(), MESSAGE_TEXT_CAP_BYTES);
    }

    #[test]
    fn publishes_under_the_claude_code_session_key() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join(format!("{SESSION}.jsonl"));
        write_transcript(
            &main,
            &[message_line(
                "user",
                "u-1",
                "2026-07-10T16:00:00.000Z",
                serde_json::json!("index me"),
                &[],
            )],
        );

        let (shard, cursors) = extract_claude_session(SESSION, &main, &[]).unwrap();
        let store = Store::open(&dir.path().join("store")).unwrap();
        let key = format!("{}:{SESSION}", Source::ClaudeCode.as_str());
        assert!(matches!(
            store.publish_session(&key, &shard, cursors, false).unwrap(),
            PublishOutcome::Published
        ));
        let read = store.snapshot().read_shard(&key).unwrap();
        assert_eq!(read.records.len(), 1);
        assert_eq!(read.records[0].text, "index me");
        assert!(read.marks.is_empty());
    }

    /// The post-fold TOCTOU guard: a fold attempted with a saved cursor
    /// whose windows no longer describe the file (the "rewrite landed
    /// after the caller validated" interleaving, pinned here by simply
    /// rewriting before the call) must be discarded — `Ok(None)` — never
    /// spliced and legitimized.
    #[test]
    fn fold_discards_when_the_saved_cursor_no_longer_matches_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join(format!("{SESSION}.jsonl"));
        write_transcript(
            &main,
            &[message_line(
                "user",
                "u-1",
                "2026-07-15T12:00:00.000Z",
                serde_json::json!("original history"),
                &[],
            )],
        );
        let (prior, cursors) = extract_claude_session(SESSION, &main, &[]).unwrap();
        let saved = cursors[0].clone();

        // The file is rewritten (head mutated, grown) after validation
        // "happened": the fold must refuse to splice.
        write_transcript(
            &main,
            &[
                message_line(
                    "user",
                    "u-9",
                    "2026-07-15T12:05:00.000Z",
                    serde_json::json!("replaced history"),
                    &[],
                ),
                message_line(
                    "user",
                    "u-10",
                    "2026-07-15T12:06:00.000Z",
                    serde_json::json!("second replaced"),
                    &[],
                ),
            ],
        );
        assert!(
            fold_claude_main_append(SESSION, &main, &saved, &prior)
                .unwrap()
                .is_none(),
            "a fold against a moved source must be discarded"
        );
    }

    /// The incremental fold's soundness invariant: for a main-only append
    /// with an unchanged subagent set, the folded shard and cursor are
    /// exactly what a full re-extraction would produce.
    #[test]
    fn fold_of_main_append_matches_full_reextraction() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join(format!("{SESSION}.jsonl"));
        let subagent_dir = dir.path().join(SESSION).join("subagents");
        std::fs::create_dir_all(&subagent_dir).unwrap();
        let agent = subagent_dir.join("agent-a1.jsonl");
        write_transcript(
            &main,
            &[
                message_line(
                    "user",
                    "u-1",
                    "2026-07-10T10:00:00.000Z",
                    serde_json::json!("first question"),
                    &[],
                ),
                message_line(
                    "assistant",
                    "a-1",
                    "2026-07-10T10:00:05.000Z",
                    serde_json::json!("first answer"),
                    &[],
                ),
            ],
        );
        write_transcript(
            &agent,
            &[sidechain_line(
                "assistant",
                "s-1",
                "2026-07-10T10:00:06.000Z",
                "subagent prose",
                "a1",
            )],
        );
        let agents = vec![agent.clone()];
        let (prior, prior_cursors) = extract_claude_session(SESSION, &main, &agents).unwrap();
        let main_cursor = prior_cursors
            .iter()
            .find(|cursor| cursor.path == main)
            .unwrap()
            .clone();

        // Append two records to the MAIN transcript only.
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&main)
            .unwrap();
        writeln!(
            file,
            "{}",
            message_line(
                "user",
                "u-2",
                "2026-07-10T10:01:00.000Z",
                serde_json::json!("second question"),
                &[],
            )
        )
        .unwrap();
        writeln!(
            file,
            "{}",
            message_line(
                "assistant",
                "a-2",
                "2026-07-10T10:01:05.000Z",
                serde_json::json!("second answer"),
                &[],
            )
        )
        .unwrap();
        drop(file);
        assert_eq!(main_cursor.check(), CursorCheck::Appended);

        let (folded, folded_cursor) = fold_claude_main_append(SESSION, &main, &main_cursor, &prior)
            .unwrap()
            .expect("main-only append is foldable");
        let (full, full_cursors) = extract_claude_session(SESSION, &main, &agents).unwrap();
        assert_eq!(folded.records, full.records);
        assert_eq!(folded.marks, full.marks);
        assert_eq!(
            Some(&folded_cursor),
            full_cursors.iter().find(|cursor| cursor.path == main)
        );
        let texts: Vec<&str> = folded.records.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "first question",
                "first answer",
                "second question",
                "second answer",
                "subagent prose",
            ]
        );
    }
}
