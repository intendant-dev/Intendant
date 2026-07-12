//! B2: the intendant-session extractor (message-search plan §5, intendant
//! lane). Walks ONE session log dir — `session.jsonl` plus the `turns/`
//! sidecars its events reference — and derives that session's
//! [`SessionShard`] + cursors. Pure per-dir: no home/env resolution here;
//! the wiring edge enumerates the logs root and decides publishing.
//!
//! Two eras coexist on disk (plan §4):
//! - **New era** (post-F1 binaries): canonical `conversation_message`
//!   rows — user text inline, assistant text as a sidecar span shared
//!   with the diagnostic `model_response` event. `conversation_rewound`
//!   rows become [`SupersessionMark::SeqCut`]s.
//! - **Legacy era** (pre-F1 logs, or the pre-marker segment of a resumed
//!   session): user text is reconstructed best-effort from
//!   `session_started` tasks (falling back to `session_meta.json` — a
//!   top-level session has ONLY the meta copy), delivered steers
//!   (`steer_requested` ⋈ `steer_delivered` on id), and
//!   `"Round {N} follow-up:"` info lines; assistant text from
//!   `model_response` sidecar spans. askHuman answers were never
//!   persisted pre-F1 — none exist to extract.
//!
//! The `conversation_message_epoch` marker is the era boundary: legacy
//! extraction stops STRICTLY at the first marker, and its
//! `(seq, role, content-hash)` mapping assigns seqs to legacy-extracted
//! records so rewind cuts cover them (hash mismatch — preludes, images —
//! just leaves a record uncorrelated: it stays active, never wrongly
//! superseded).
//!
//! Wrapper sessions (any `session_identity` event) are skipped entirely:
//! their `model_response` events mirror messages that are canonical in
//! the external backend's own log (the Codex/Claude extractors' lane) —
//! extracting them here would double every wrapped message.

use super::cursor::{read_complete_lines_from, SourceCursor};
use super::record::{
    cap_text, Locator, MessageRecord, Role, Source, SupersessionMark, MESSAGE_TEXT_CAP_BYTES,
};
use super::store::SessionShard;
use crate::session_log::{content_hash_hex16, SessionMeta};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path};

/// One session dir's extraction. `wrapper` sessions deliberately carry an
/// empty shard (see the module doc); the cursor is still captured so the
/// wiring edge can avoid rescanning a large wrapper log every sweep.
pub(crate) struct IntendantExtraction {
    pub shard: SessionShard,
    pub cursors: Vec<SourceCursor>,
    pub wrapper: bool,
}

/// Extract one intendant session log dir. Missing `session.jsonl` yields
/// an empty extraction (a dir that never logged has nothing to index).
pub(crate) fn extract_intendant_session(log_dir: &Path) -> std::io::Result<IntendantExtraction> {
    let session_jsonl = log_dir.join("session.jsonl");
    if !session_jsonl.exists() {
        return Ok(IntendantExtraction {
            shard: SessionShard::default(),
            cursors: Vec::new(),
            wrapper: false,
        });
    }
    let (lines, consumed) = read_complete_lines_from(&session_jsonl, 0)?;
    let meta = read_meta(log_dir);
    let session_id = log_dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .or_else(|| meta.as_ref().map(|meta| meta.session_id.clone()))
        .unwrap_or_else(|| "unknown".to_string());

    // Single parse pass; era decisions need whole-file facts (marker
    // position, wrapper flag, whether any canonical rows exist at all).
    let mut events: Vec<(u64, serde_json::Value)> = Vec::new();
    let mut wrapper = false;
    let mut first_marker: Option<usize> = None;
    let mut any_conversation_message = false;
    for (index, line) in lines.iter().enumerate() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue; // torn or foreign line: not extractable, never fatal
        };
        match value.get("event").and_then(|event| event.as_str()) {
            Some("session_identity") => wrapper = true,
            Some("conversation_message_epoch") => {
                first_marker.get_or_insert(events.len());
            }
            Some("conversation_message") => any_conversation_message = true,
            _ => {}
        }
        events.push((index as u64 + 1, value));
    }

    let cursors = SourceCursor::capture(&session_jsonl, consumed)
        .map(|cursor| vec![cursor])
        .unwrap_or_default();
    if wrapper {
        return Ok(IntendantExtraction {
            shard: SessionShard::default(),
            cursors,
            wrapper: true,
        });
    }

    // Era rule: a marker splits explicitly; without one, the presence of
    // any canonical row means the session was born new-era (its
    // session_started / follow-up info lines still exist as diagnostics
    // and MUST NOT be extracted a second time).
    let legacy_end = match first_marker {
        Some(marker_index) => marker_index,
        None if any_conversation_message => 0,
        None => events.len(),
    };

    let mut records: Vec<MessageRecord> = Vec::new();
    let mut marks: Vec<SupersessionMark> = Vec::new();
    // Full-text hashes of legacy records (index-aligned) for epoch-mapping
    // correlation; computed BEFORE capping so they match canonical hashes.
    let mut legacy_hashes: Vec<Option<String>> = Vec::new();

    let mut dater = LegacyDater::new(meta.as_ref());
    let mut pending_steers: Vec<(String, String, u64)> = Vec::new(); // (id, text, line_no)
    let mut legacy_task_seen = false;

    let record_base = |role: Role, ts_ms: i64, text: String, locator: Locator| {
        let (text, truncated) = cap_text(text);
        MessageRecord {
            source: Source::Intendant,
            session_id: session_id.clone(),
            role,
            ts_ms,
            text,
            locator,
            seq: None,
            user_turn: None,
            item_id: None,
            subagent: false,
            generation: 0,
            truncated,
        }
    };

    for (position, (line_no, event)) in events.iter().enumerate() {
        let name = event.get("event").and_then(|event| event.as_str());
        let data = event.get("data");
        let ts_ms_field = event.get("ts_ms").and_then(|ts| ts.as_i64());
        match name {
            // ---- Canonical lane (any position; see the era rule) ----
            Some("conversation_message") => {
                let Some(data) = data else { continue };
                let (Some(message_id), Some(seq), Some(role)) = (
                    data.get("message_id").and_then(|id| id.as_str()),
                    data.get("message_seq").and_then(|seq| seq.as_u64()),
                    data.get("role").and_then(|role| role.as_str()),
                ) else {
                    continue;
                };
                let ts_ms = ts_ms_field.unwrap_or(0);
                let locator = Locator::NativeMessageId {
                    message_id: message_id.to_string(),
                };
                let mut record = match role {
                    "user" => {
                        let Some(text) = data.get("text").and_then(|text| text.as_str()) else {
                            continue;
                        };
                        record_base(Role::User, ts_ms, text.to_string(), locator)
                    }
                    "assistant" => {
                        let Some(text) = read_event_span(log_dir, event, data) else {
                            continue; // span unreadable: anomaly, skip
                        };
                        record_base(Role::Assistant, ts_ms, text, locator)
                    }
                    _ => continue,
                };
                record.seq = Some(seq);
                records.push(record);
                legacy_hashes.push(None);
            }
            Some("conversation_rewound") => {
                let Some(data) = data else { continue };
                let Some(cut_after_seq) = data.get("cut_after_seq").and_then(|seq| seq.as_u64())
                else {
                    continue;
                };
                let at_ms = data
                    .get("superseded_at_ms")
                    .and_then(|at| at.as_i64())
                    .or(ts_ms_field)
                    .unwrap_or(0);
                marks.push(SupersessionMark::SeqCut {
                    cut_after_seq,
                    at_ms,
                });
            }
            // ---- Legacy lane (strictly before the era boundary) ----
            Some("session_started") if position < legacy_end => {
                let ts_ms = dater.date_row(ts_ms_field, event);
                let task = data
                    .and_then(|data| data.get("task"))
                    .and_then(|task| task.as_str());
                if let Some(task) = task {
                    legacy_task_seen = true;
                    push_legacy(
                        &mut records,
                        &mut legacy_hashes,
                        record_base(
                            Role::User,
                            ts_ms,
                            task.to_string(),
                            Locator::NativeEvent {
                                line_no: *line_no,
                                content_hash16: content_hash_hex16(task),
                            },
                        ),
                        content_hash_hex16(task),
                    );
                }
            }
            Some("steer_requested") if position < legacy_end => {
                let Some(data) = data else { continue };
                if let (Some(id), Some(text)) = (
                    data.get("id").and_then(|id| id.as_str()),
                    data.get("text").and_then(|text| text.as_str()),
                ) {
                    pending_steers.push((id.to_string(), text.to_string(), *line_no));
                }
            }
            Some("steer_delivered") if position < legacy_end => {
                let Some(id) = data
                    .and_then(|data| data.get("id"))
                    .and_then(|id| id.as_str())
                else {
                    continue;
                };
                let Some(slot) = pending_steers.iter().position(|(known, _, _)| known == id)
                else {
                    continue; // delivered without a seen request: nothing to say
                };
                let (_, text, requested_line) = pending_steers.remove(slot);
                let ts_ms = dater.date_row(ts_ms_field, event);
                let hash = content_hash_hex16(&text);
                push_legacy(
                    &mut records,
                    &mut legacy_hashes,
                    record_base(
                        Role::User,
                        ts_ms,
                        text,
                        Locator::NativeEvent {
                            line_no: requested_line,
                            content_hash16: hash.clone(),
                        },
                    ),
                    hash,
                );
            }
            Some("info") if position < legacy_end => {
                let Some(text) = event
                    .get("message")
                    .and_then(|message| message.as_str())
                    .and_then(parse_round_follow_up)
                else {
                    continue;
                };
                let ts_ms = dater.date_row(ts_ms_field, event);
                let hash = content_hash_hex16(&text);
                push_legacy(
                    &mut records,
                    &mut legacy_hashes,
                    record_base(
                        Role::User,
                        ts_ms,
                        text,
                        Locator::NativeEvent {
                            line_no: *line_no,
                            content_hash16: hash.clone(),
                        },
                    ),
                    hash,
                );
            }
            Some("model_response") if position < legacy_end => {
                let Some(data) = data else { continue };
                let Some(text) = read_event_span(log_dir, event, data) else {
                    continue;
                };
                let (Some(file), Some(offset), Some(len)) = (
                    event.get("file").and_then(|file| file.as_str()),
                    data.get("model_offset").and_then(|offset| offset.as_u64()),
                    data.get("model_bytes").and_then(|len| len.as_u64()),
                ) else {
                    continue;
                };
                let ts_ms = dater.date_row(ts_ms_field, event);
                let hash = content_hash_hex16(&text);
                push_legacy(
                    &mut records,
                    &mut legacy_hashes,
                    record_base(
                        Role::Assistant,
                        ts_ms,
                        text,
                        Locator::NativeSidecarSpan {
                            file: file.to_string(),
                            offset,
                            len,
                            content_hash16: hash.clone(),
                        },
                    ),
                    hash,
                );
            }
            _ => {}
        }
    }

    // Top-level legacy sessions carry the task only in session_meta.json.
    if legacy_end == events.len() && !legacy_task_seen {
        if let Some(task) = meta.as_ref().and_then(|meta| meta.task.as_deref()) {
            let ts_ms = dater.meta_ts_ms();
            let hash = content_hash_hex16(task);
            // line_no 0 = "no event line; sourced from session_meta.json".
            push_legacy(
                &mut records,
                &mut legacy_hashes,
                record_base(
                    Role::User,
                    ts_ms,
                    task.to_string(),
                    Locator::NativeEvent {
                        line_no: 0,
                        content_hash16: hash.clone(),
                    },
                ),
                hash,
            );
        }
    }

    // Epoch correlation: assign seqs to legacy records by (role, hash),
    // greedily in mapping order (both sides are in conversation order).
    for (_, event) in &events {
        if event.get("event").and_then(|event| event.as_str()) != Some("conversation_message_epoch")
        {
            continue;
        }
        let Some(mapping) = event
            .get("data")
            .and_then(|data| data.get("mapping"))
            .and_then(|mapping| mapping.as_array())
        else {
            continue;
        };
        for row in mapping {
            let (Some(seq), Some(role), Some(hash)) = (
                row.get(0).and_then(|seq| seq.as_u64()),
                row.get(1).and_then(|role| role.as_str()),
                row.get(2).and_then(|hash| hash.as_str()),
            ) else {
                continue;
            };
            let role = match role {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => continue,
            };
            if let Some(index) = (0..records.len()).find(|&index| {
                records[index].seq.is_none()
                    && records[index].role == role
                    && legacy_hashes[index].as_deref() == Some(hash)
            }) {
                records[index].seq = Some(seq);
            }
        }
    }

    Ok(IntendantExtraction {
        shard: SessionShard { records, marks },
        cursors,
        wrapper: false,
    })
}

fn push_legacy(
    records: &mut Vec<MessageRecord>,
    hashes: &mut Vec<Option<String>>,
    record: MessageRecord,
    full_text_hash: String,
) {
    records.push(record);
    hashes.push(Some(full_text_hash));
}

fn read_meta(log_dir: &Path) -> Option<SessionMeta> {
    let raw = std::fs::read_to_string(log_dir.join("session_meta.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Read the sidecar span an event references (`file` + `data.model_offset`
/// + `data.model_bytes`). The read is bounded a little past the record
/// text cap — a corrupt length must not balloon memory; the overlong tail
/// is dropped by [`cap_text`] anyway. A short read (bytes the event
/// promised aren't there) is an anomaly: skip, don't guess.
fn read_event_span(
    log_dir: &Path,
    event: &serde_json::Value,
    data: &serde_json::Value,
) -> Option<String> {
    let file = event.get("file").and_then(|file| file.as_str())?;
    let offset = data.get("model_offset").and_then(|offset| offset.as_u64())?;
    let len = data.get("model_bytes").and_then(|len| len.as_u64())?;
    let relative = Path::new(file);
    // The log dir is the trust boundary: never follow an absolute or
    // parent-escaping reference out of it.
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return None;
    }
    let bounded = len.min(MESSAGE_TEXT_CAP_BYTES as u64 + 8);
    let mut spanned = vec![0u8; bounded as usize];
    let mut handle = std::fs::File::open(log_dir.join(relative)).ok()?;
    handle.seek(SeekFrom::Start(offset)).ok()?;
    handle.read_exact(&mut spanned).ok()?;
    Some(String::from_utf8_lossy(&spanned).into_owned())
}

/// Parse a legacy `"Round {N} follow-up: {text}"` info line; the optional
/// `" ({n} attachment(s))"` suffix the emitter appends is stripped.
/// Best-effort by design: user text that itself ends with that shape loses
/// the tail (plan §5 records this lane as best-effort).
fn parse_round_follow_up(message: &str) -> Option<String> {
    let rest = message.strip_prefix("Round ")?;
    let space = rest.find(' ')?;
    if rest[..space].is_empty() || !rest[..space].bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let text = rest[space..].strip_prefix(" follow-up: ")?;
    if let Some(head) = text.strip_suffix(" attachment(s))") {
        if let Some(open) = head.rfind(" (") {
            if head[open + 2..].bytes().all(|byte| byte.is_ascii_digit())
                && !head[open + 2..].is_empty()
            {
                return Some(head[..open].to_string());
            }
        }
    }
    Some(text.to_string())
}

/// Dates legacy rows (plan §5): pre-F2 events carry only a local
/// time-of-day `ts`, so the calendar date comes from
/// `session_meta.json`'s `created_at` plus midnight-wrap inference (the
/// date advances whenever the time-of-day decreases between consecutive
/// dated rows). Naive datetimes are interpreted as UTC unless the meta's
/// `created_at_ms` reveals the session's true offset — deterministic
/// everywhere, and at worst hours off on pre-2026-07 sessions, which only
/// nudges the 14-day retention edge. Rows carrying `ts_ms` use it as-is.
struct LegacyDater {
    current: Option<chrono::NaiveDateTime>,
    offset_ms: i64,
    meta_ms: i64,
}

impl LegacyDater {
    fn new(meta: Option<&SessionMeta>) -> Self {
        let created_naive = meta.and_then(|meta| {
            chrono::NaiveDateTime::parse_from_str(&meta.created_at, "%Y-%m-%dT%H:%M:%S").ok()
        });
        let created_ms = meta.and_then(|meta| meta.created_at_ms);
        let offset_ms = match (created_naive, created_ms) {
            (Some(naive), Some(ms)) => ms - naive.and_utc().timestamp_millis(),
            _ => 0,
        };
        let meta_ms = created_ms
            .or_else(|| created_naive.map(|naive| naive.and_utc().timestamp_millis()))
            .unwrap_or(0);
        Self {
            current: created_naive,
            offset_ms,
            meta_ms,
        }
    }

    /// The best timestamp for a record sourced from the meta itself.
    fn meta_ts_ms(&self) -> i64 {
        self.meta_ms
    }

    fn date_row(&mut self, ts_ms: Option<i64>, event: &serde_json::Value) -> i64 {
        if let Some(ts_ms) = ts_ms {
            return ts_ms;
        }
        let Some(tod) = event
            .get("ts")
            .and_then(|ts| ts.as_str())
            .and_then(parse_time_of_day)
        else {
            return self.meta_ms;
        };
        let Some(current) = self.current else {
            // No meta anchor at all (broken dir): undatable, excluded by
            // retention rather than invented.
            return 0;
        };
        let date = if tod < current.time() {
            current
                .date()
                .succ_opt()
                .unwrap_or_else(|| current.date())
        } else {
            current.date()
        };
        let stamped = date.and_time(tod);
        self.current = Some(stamped);
        stamped.and_utc().timestamp_millis() + self.offset_ms
    }
}

fn parse_time_of_day(ts: &str) -> Option<chrono::NaiveTime> {
    chrono::NaiveTime::parse_from_str(ts, "%H:%M:%S%.3f")
        .or_else(|_| chrono::NaiveTime::parse_from_str(ts, "%H:%M:%S"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::super::cursor::CursorCheck;
    use super::super::record::derive_active;
    use super::*;
    use std::io::Write;

    fn write_lines(dir: &Path, lines: &[serde_json::Value]) {
        let mut body = String::new();
        for line in lines {
            body.push_str(&line.to_string());
            body.push('\n');
        }
        std::fs::write(dir.join("session.jsonl"), body).unwrap();
    }

    fn write_meta(dir: &Path, created_at: &str, created_at_ms: Option<i64>, task: Option<&str>) {
        let mut meta = serde_json::json!({
            "session_id": "meta-id",
            "created_at": created_at,
        });
        if let Some(ms) = created_at_ms {
            meta["created_at_ms"] = serde_json::Value::from(ms);
        }
        if let Some(task) = task {
            meta["task"] = serde_json::Value::from(task);
        }
        std::fs::write(dir.join("session_meta.json"), meta.to_string()).unwrap();
    }

    fn sidecar(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir.join("turns")).unwrap();
        std::fs::write(dir.join("turns").join(name), body).unwrap();
    }

    fn conversation_message_user(seq: u64, ts_ms: i64, text: &str) -> serde_json::Value {
        serde_json::json!({
            "ts": "10:00:00.000", "ts_ms": ts_ms, "event": "conversation_message",
            "data": {"message_id": format!("mid-{seq}"), "message_seq": seq,
                     "role": "user", "provenance": "task", "text": text},
        })
    }

    fn conversation_message_assistant(
        seq: u64,
        ts_ms: i64,
        file: &str,
        offset: u64,
        len: u64,
    ) -> serde_json::Value {
        serde_json::json!({
            "ts": "10:00:01.000", "ts_ms": ts_ms, "event": "conversation_message",
            "file": file,
            "data": {"message_id": format!("mid-{seq}"), "message_seq": seq,
                     "role": "assistant", "provenance": "assistant",
                     "model_offset": offset, "model_bytes": len},
        })
    }

    fn model_response(ts: &str, file: &str, offset: u64, len: u64) -> serde_json::Value {
        serde_json::json!({
            "ts": ts, "event": "model_response",
            "file": file,
            "data": {"model_offset": offset, "model_bytes": len, "content_length": len},
        })
    }

    #[test]
    fn new_era_session_extracts_canonical_rows_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sess-new");
        std::fs::create_dir_all(&dir).unwrap();
        write_meta(&dir, "2026-07-01T10:00:00", Some(1_780_000_000_000), None);
        sidecar(&dir, "turn_001_model.txt", "HELLOFULL ASSISTANT TEXT");
        write_lines(
            &dir,
            &[
                // Diagnostics that must NOT double-extract in a new-era log:
                serde_json::json!({"ts":"10:00:00.000","ts_ms":1,"event":"session_started",
                    "data":{"session_id":"sess-new","task":"build the thing"}}),
                serde_json::json!({"ts":"10:00:00.000","ts_ms":1,"event":"info",
                    "message":"Round 2 follow-up: also this"}),
                model_response("10:00:01.000", "turns/turn_001_model.txt", 5, 19),
                conversation_message_user(1, 1_000, "build the thing"),
                conversation_message_assistant(2, 2_000, "turns/turn_001_model.txt", 5, 19),
                conversation_message_user(3, 3_000, "scratch that"),
                serde_json::json!({"ts":"10:00:05.000","ts_ms":4_000,"event":"conversation_rewound",
                    "data":{"cut_after_seq":2,"kind":"tail_rollback","superseded_at_ms":4_000}}),
            ],
        );

        let out = extract_intendant_session(&dir).unwrap();
        assert!(!out.wrapper);
        let texts: Vec<&str> = out
            .shard
            .records
            .iter()
            .map(|record| record.text.as_str())
            .collect();
        assert_eq!(
            texts,
            vec!["build the thing", "FULL ASSISTANT TEXT", "scratch that"],
            "canonical rows only — the legacy diagnostics are not re-extracted"
        );
        assert_eq!(out.shard.records[0].seq, Some(1));
        assert_eq!(out.shard.records[1].role, Role::Assistant);
        assert_eq!(
            out.shard.records[1].locator,
            Locator::NativeMessageId {
                message_id: "mid-2".into()
            }
        );
        assert_eq!(out.shard.records[2].ts_ms, 3_000);
        assert_eq!(out.shard.marks.len(), 1);
        let active = derive_active(&out.shard.records, &out.shard.marks);
        assert_eq!(active, vec![true, true, false], "seq 3 superseded by the cut");
        assert_eq!(out.cursors.len(), 1);
        assert_eq!(out.cursors[0].check(), CursorCheck::Unchanged);
    }

    #[test]
    fn wrapper_sessions_yield_an_empty_shard_with_a_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sess-wrap");
        std::fs::create_dir_all(&dir).unwrap();
        sidecar(&dir, "turn_001_model.txt", "wrapped text");
        write_lines(
            &dir,
            &[
                serde_json::json!({"ts":"09:00:00.000","ts_ms":1,"event":"session_identity",
                    "data":{"session_id":"sess-wrap","source":"codex","backend_session_id":"abc"}}),
                model_response("09:00:01.000", "turns/turn_001_model.txt", 0, 12),
            ],
        );
        let out = extract_intendant_session(&dir).unwrap();
        assert!(out.wrapper);
        assert!(out.shard.records.is_empty());
        assert_eq!(out.cursors.len(), 1, "cursor still captured for sweep skip");
    }

    #[test]
    fn legacy_session_reconstructs_user_lane_and_spans() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sess-old");
        std::fs::create_dir_all(&dir).unwrap();
        write_meta(&dir, "2026-01-01T23:59:00", None, None);
        sidecar(&dir, "turn_001_model.txt", "AABBBB");
        write_lines(
            &dir,
            &[
                serde_json::json!({"ts":"23:59:01.000","event":"session_started",
                    "data":{"session_id":"sess-old","task":"the legacy task"}}),
                serde_json::json!({"ts":"23:59:02.000","event":"steer_requested",
                    "data":{"id":"st-1","status":"pending","text":"go left"}}),
                serde_json::json!({"ts":"23:59:03.000","event":"steer_requested",
                    "data":{"id":"st-2","status":"pending","text":"never delivered"}}),
                model_response("23:59:30.000", "turns/turn_001_model.txt", 0, 2),
                // Past midnight: the date must wrap forward.
                serde_json::json!({"ts":"00:00:05.000","event":"steer_delivered",
                    "data":{"id":"st-1","status":"delivered","mid_turn":false}}),
                serde_json::json!({"ts":"00:01:00.000","event":"info",
                    "message":"Round 2 follow-up: try the other door (2 attachment(s))"}),
                model_response("00:01:30.000", "turns/turn_001_model.txt", 2, 4),
            ],
        );

        let out = extract_intendant_session(&dir).unwrap();
        let records = &out.shard.records;
        let texts: Vec<&str> = records.iter().map(|record| record.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "the legacy task",
                "AA",
                "go left",
                "try the other door",
                "BBBB"
            ]
        );
        // Undelivered steer produced nothing.
        assert!(!texts.contains(&"never delivered"));

        // Dating: naive-as-UTC off created_at 2026-01-01, wrapping at midnight.
        let day1 = chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let day2 = chrono::NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        let expect = |date: chrono::NaiveDate, time: &str| {
            date.and_time(chrono::NaiveTime::parse_from_str(time, "%H:%M:%S%.3f").unwrap())
                .and_utc()
                .timestamp_millis()
        };
        assert_eq!(records[0].ts_ms, expect(day1, "23:59:01.000"));
        assert_eq!(records[2].ts_ms, expect(day2, "00:00:05.000"), "wrapped");
        assert_eq!(records[3].ts_ms, expect(day2, "00:01:00.000"));

        // Locators: steer anchors on the REQUESTED line (where the text is).
        assert_eq!(
            records[2].locator,
            Locator::NativeEvent {
                line_no: 2,
                content_hash16: content_hash_hex16("go left"),
            }
        );
        assert_eq!(
            records[4].locator,
            Locator::NativeSidecarSpan {
                file: "turns/turn_001_model.txt".into(),
                offset: 2,
                len: 4,
                content_hash16: content_hash_hex16("BBBB"),
            }
        );
        // No marks in a legacy session.
        assert!(out.shard.marks.is_empty());
    }

    #[test]
    fn legacy_top_level_task_falls_back_to_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sess-meta");
        std::fs::create_dir_all(&dir).unwrap();
        write_meta(
            &dir,
            "2026-01-05T08:00:00",
            Some(1_767_600_000_000),
            Some("meta-only task"),
        );
        write_lines(
            &dir,
            &[serde_json::json!({"ts":"08:00:01.000","event":"info","message":"hello"})],
        );
        let out = extract_intendant_session(&dir).unwrap();
        assert_eq!(out.shard.records.len(), 1);
        assert_eq!(out.shard.records[0].text, "meta-only task");
        assert_eq!(out.shard.records[0].ts_ms, 1_767_600_000_000);
        assert_eq!(
            out.shard.records[0].locator,
            Locator::NativeEvent {
                line_no: 0,
                content_hash16: content_hash_hex16("meta-only task"),
            },
            "line 0 marks a meta-sourced record"
        );
    }

    #[test]
    fn epoch_marker_splits_eras_and_correlates_seqs() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sess-mixed");
        std::fs::create_dir_all(&dir).unwrap();
        write_meta(&dir, "2026-06-01T10:00:00", None, None);
        sidecar(&dir, "turn_001_model.txt", "legacy answer");
        write_lines(
            &dir,
            &[
                serde_json::json!({"ts":"10:00:01.000","event":"session_started",
                    "data":{"session_id":"sess-mixed","task":"old task"}}),
                model_response("10:00:02.000", "turns/turn_001_model.txt", 0, 13),
                serde_json::json!({"ts":"11:00:00.000","ts_ms":100,"event":"conversation_message_epoch",
                    "data":{"mapping":[
                        [1, "system", "ffffffffffffffff"],
                        [2, "user", content_hash_hex16("old task")],
                        [3, "assistant", content_hash_hex16("legacy answer")],
                    ]}}),
                conversation_message_user(4, 200, "post-resume message"),
                // Legacy-shaped rows AFTER the marker must be ignored:
                model_response("11:00:02.000", "turns/turn_001_model.txt", 0, 13),
                serde_json::json!({"ts":"11:00:03.000","ts_ms":300,"event":"conversation_rewound",
                    "data":{"cut_after_seq":2,"kind":"tail_rollback","superseded_at_ms":300}}),
            ],
        );

        let out = extract_intendant_session(&dir).unwrap();
        let records = &out.shard.records;
        assert_eq!(records.len(), 3, "legacy task + legacy span + one canonical");
        assert_eq!(records[0].seq, Some(2), "correlated through the mapping");
        assert_eq!(records[1].seq, Some(3));
        assert_eq!(records[2].seq, Some(4));
        let active = derive_active(records, &out.shard.marks);
        assert_eq!(
            active,
            vec![true, false, false],
            "the cut after seq 2 supersedes the correlated legacy assistant too"
        );
    }

    #[test]
    fn rows_with_ts_ms_use_it_directly() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sess-f2");
        std::fs::create_dir_all(&dir).unwrap();
        write_meta(&dir, "2026-01-01T00:00:00", None, None);
        write_lines(
            &dir,
            &[serde_json::json!({"ts":"12:00:00.000","ts_ms":42_000,"event":"session_started",
                "data":{"session_id":"sess-f2","task":"dated task"}})],
        );
        let out = extract_intendant_session(&dir).unwrap();
        assert_eq!(out.shard.records[0].ts_ms, 42_000);
    }

    #[test]
    fn partial_trailing_line_stays_unread_and_missing_log_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sess-partial");
        std::fs::create_dir_all(&dir).unwrap();
        write_meta(&dir, "2026-01-01T00:00:00", None, None);
        let full = conversation_message_user(1, 1_000, "whole line").to_string();
        let mut file = std::fs::File::create(dir.join("session.jsonl")).unwrap();
        writeln!(file, "{full}").unwrap();
        write!(file, "{{\"event\":\"conversation_message\",\"data\":{{\"te").unwrap();
        drop(file);

        let out = extract_intendant_session(&dir).unwrap();
        assert_eq!(out.shard.records.len(), 1);
        assert_eq!(
            out.cursors[0].last_complete_line_offset,
            full.len() as u64 + 1
        );
        assert_eq!(out.cursors[0].check(), CursorCheck::Appended);

        let empty = tmp.path().join("sess-none");
        std::fs::create_dir_all(&empty).unwrap();
        let out = extract_intendant_session(&empty).unwrap();
        assert!(out.shard.records.is_empty());
        assert!(out.cursors.is_empty());
    }

    #[test]
    fn span_reads_never_escape_the_log_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sess-escape");
        std::fs::create_dir_all(&dir).unwrap();
        write_meta(&dir, "2026-01-01T00:00:00", None, None);
        std::fs::write(tmp.path().join("outside.txt"), "SECRET").unwrap();
        write_lines(
            &dir,
            &[
                serde_json::json!({"ts":"10:00:00.000","event":"model_response",
                    "file":"../outside.txt",
                    "data":{"model_offset":0,"model_bytes":6}}),
                serde_json::json!({"ts":"10:00:01.000","event":"model_response",
                    "file":"/etc/hosts",
                    "data":{"model_offset":0,"model_bytes":4}}),
            ],
        );
        let out = extract_intendant_session(&dir).unwrap();
        assert!(out.shard.records.is_empty());
    }

    #[test]
    fn round_follow_up_parsing_is_strict_about_shape() {
        assert_eq!(
            parse_round_follow_up("Round 3 follow-up: plain text"),
            Some("plain text".to_string())
        );
        assert_eq!(
            parse_round_follow_up("Round 3 follow-up: with files (12 attachment(s))"),
            Some("with files".to_string())
        );
        assert_eq!(parse_round_follow_up("Round x follow-up: nope"), None);
        assert_eq!(parse_round_follow_up("Skipped cancelled queued follow-up"), None);
        assert_eq!(parse_round_follow_up("Round 12 complete (3 turns)"), None);
    }
}
