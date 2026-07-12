//! `locate=` anchored session-detail reads (message-search plan §7, C2).
//!
//! A message-search hit carries an opaque, versioned [`Locator`] minted by
//! the extractors (`message_search/record.rs` — FROZEN). This module
//! resolves one against the live session sources and answers the normal
//! session-detail body plus an additive `locate` object:
//!
//! ```json
//! "locate": { "state": "resolved", "entry_index": 3, "total_index": 41,
//!             "anchor": "exact" }
//! "locate": { "state": "stale",       "reason": "…" }
//! "locate": { "state": "unavailable", "reason": "…" }
//! ```
//!
//! - `resolved`: the served page contains the located event;
//!   `entry_index` is its position in this response's `entries` array,
//!   `total_index` its position in the full entry list (the existing
//!   `page_start`/`page_end`/`total_entries` vocabulary), and `anchor` is
//!   `"exact"` when the entry renders the located source row itself,
//!   `"nearest"` when the row renders no entry of its own (e.g. canonical
//!   `conversation_message` rows, whose diagnostic twins are what the
//!   detail view shows) and the closest rendered neighbor anchors it.
//! - `stale`: the source no longer matches the locator (content hash
//!   mismatch, rewritten/rolled-back external thread, truncated log). The
//!   page is served exactly as an unanchored request; the dashboard opens
//!   the detail view unanchored and says why.
//! - `unavailable`: the locator is well-formed but nothing resolvable
//!   backs it (missing file/record, locator kind vs. source mismatch,
//!   subagent-only record). Same graceful degradation.
//!
//! A malformed `locate` parameter (undecodable, unknown `kind`) is a
//! request error — the endpoint answers 400 like any bad parameter —
//! whereas every well-formed locator degrades typed. Nothing in this
//! module panics on hostile input; sidecar reads never escape the session
//! directory (mirroring the extractor's span reads).
//!
//! Wire format of the parameter: either the locator JSON itself (the
//! object message-search hits carry, URL-encoded on the HTTP lane) or
//! base64url (padded or not) of that JSON. The dashboard-control twin
//! (`api_session_detail`) may also pass the JSON object directly in its
//! params; the transport edge stringifies it before parsing.

use super::*;
use crate::message_search::{parse_round_follow_up, Locator, MESSAGE_TEXT_CAP_BYTES};
use crate::session_log::content_hash_hex16;
use base64::Engine as _;
use std::io::SeekFrom;

/// Cap on the encoded `locate` parameter. Locators are small (the largest
/// carries a ≤512-byte sidecar path plus offsets); anything bigger is not
/// a locator.
pub(crate) const LOCATE_PARAM_MAX_BYTES: usize = 8 * 1024;

/// Marker key used to find the anchored entry again after paging (the
/// page keeps always-included metadata rows, so positions shift). Never
/// serialized: it is removed before the body is assembled.
const LOCATE_ANCHOR_TAG: &str = "__locate_anchor";

/// Decode the `locate` request parameter into a [`Locator`]. Accepts the
/// raw locator JSON (`{"kind":…}`) or base64url of it (URL-safe alphabet,
/// padding optional). Errors are request errors (400), not resolution
/// states.
pub(crate) fn parse_locate_param(raw: &str) -> Result<Locator, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty locate parameter".to_string());
    }
    if raw.len() > LOCATE_PARAM_MAX_BYTES {
        return Err("locate parameter too large".to_string());
    }
    let json = if raw.starts_with('{') {
        raw.to_string()
    } else {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(raw.trim_end_matches('=').as_bytes())
            .map_err(|_| "locate parameter is neither locator JSON nor base64url".to_string())?;
        String::from_utf8(bytes)
            .map_err(|_| "locate parameter decodes to invalid UTF-8".to_string())?
    };
    serde_json::from_str::<Locator>(&json).map_err(|error| format!("invalid locator: {error}"))
}

/// Typed resolution result (plan §7: `resolved | stale | unavailable`).
#[derive(Debug)]
pub(crate) enum LocateOutcome {
    Resolved {
        /// Index of the anchored entry in the FULL entry list.
        total_index: usize,
        /// `"exact"` or `"nearest"` (see the module doc).
        anchor: &'static str,
    },
    Stale {
        reason: String,
    },
    Unavailable {
        reason: String,
    },
}

impl LocateOutcome {
    fn stale(reason: impl Into<String>) -> Self {
        LocateOutcome::Stale {
            reason: reason.into(),
        }
    }

    fn unavailable(reason: impl Into<String>) -> Self {
        LocateOutcome::Unavailable {
            reason: reason.into(),
        }
    }
}

// ---------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------

/// Locate-aware sibling of `session_detail_response_body_with_page`: the
/// same body plus the `locate` object. Resolution failure never fails the
/// read — the page is served unanchored with the typed reason.
pub(crate) fn session_detail_response_body_with_locate(
    home: &Path,
    session_id: &str,
    source: &str,
    limit: Option<usize>,
    before: Option<usize>,
    locator: &Locator,
) -> String {
    let session_id = session_id.trim();
    if !session_lookup_id_is_safe(session_id) {
        return serde_json::json!({"error": "invalid session id"}).to_string();
    }
    let source = source.trim();
    let source = if source.is_empty() {
        "intendant"
    } else {
        source
    };
    if source == "intendant" {
        native_session_detail_with_locate(home, session_id, limit, before, locator)
    } else {
        external_session_detail_with_locate(home, source, session_id, limit, before, locator)
            .unwrap_or_else(|| serde_json::json!({"error": "session not found"}).to_string())
    }
}

fn native_session_detail_with_locate(
    home: &Path,
    session_id: &str,
    limit: Option<usize>,
    before: Option<usize>,
    locator: &Locator,
) -> String {
    let Some(session_dir) = resolve_bare_session_dir_from_home(home, session_id) else {
        return serde_json::json!({"error": "session not found"}).to_string();
    };
    let contents = std::fs::read_to_string(session_dir.join("session.jsonl")).ok();
    let (mut entries, entry_lines) = match contents.as_deref() {
        Some(contents) => {
            let (entries, entry_lines, _) =
                session_log_replay_entries_from_contents(&session_dir, contents);
            (entries, entry_lines)
        }
        // Parity with the unanchored read: a dir without session.jsonl
        // serves an empty entry list.
        None => (Vec::new(), Vec::new()),
    };
    compact_context_snapshot_entries_for_replay(&mut entries);
    let outcome = match contents.as_deref() {
        Some(contents) => native_locator_outcome(&session_dir, contents, &entry_lines, locator),
        None => LocateOutcome::unavailable("session log not found"),
    };
    let (page, locate) = page_with_locate_outcome(entries, limit, before, None, outcome);
    native_session_detail_body(&session_dir, page, Some(locate))
}

fn external_session_detail_with_locate(
    home: &Path,
    source: &str,
    session_id: &str,
    limit: Option<usize>,
    before: Option<usize>,
    locator: &Locator,
) -> Option<String> {
    let source = crate::session_names::normalize_source(source);
    let entries = external_session_entries_from_home(home, &source, session_id)?;
    let outcome = external_locator_outcome(home, &source, session_id, &entries, locator);
    let (page, locate) = page_with_locate_outcome(
        entries,
        limit,
        before,
        Some(EXTERNAL_SESSION_DETAIL_DEFAULT_ENTRY_LIMIT),
        outcome,
    );
    Some(external_session_detail_body(session_id, page, Some(locate)))
}

// ---------------------------------------------------------------------
// Paging around the anchor
// ---------------------------------------------------------------------

/// Page `entries` so a resolved anchor lands inside the served window
/// (centered on it when a limit applies), or exactly like an unanchored
/// request when resolution degraded. Returns the page plus the `locate`
/// body object.
fn page_with_locate_outcome(
    mut entries: Vec<serde_json::Value>,
    limit: Option<usize>,
    before: Option<usize>,
    default_limit: Option<usize>,
    outcome: LocateOutcome,
) -> (SessionDetailPageEntries, serde_json::Value) {
    let effective_limit = limit.or(default_limit);
    match outcome {
        LocateOutcome::Resolved {
            total_index,
            anchor,
        } if total_index < entries.len() => {
            // Center the anchor: page_end = total_index + 1 + limit/2
            // (clamped to the log) keeps it strictly inside
            // [page_start, page_end) for every limit ≥ 1. An explicit
            // `before` is superseded — anchoring is the request.
            let before = effective_limit.map(|limit| {
                let limit = limit.clamp(1, SESSION_DETAIL_ENTRY_LIMIT_MAX);
                (total_index + 1 + limit / 2).min(entries.len())
            });
            if let Some(anchor_entry) = entries
                .get_mut(total_index)
                .and_then(|entry| entry.as_object_mut())
            {
                anchor_entry.insert(LOCATE_ANCHOR_TAG.to_string(), serde_json::Value::Bool(true));
            }
            let mut page = session_detail_page_entries(entries, effective_limit, before);
            let mut entry_index: Option<usize> = None;
            for (index, entry) in page.entries.iter_mut().enumerate() {
                if let Some(obj) = entry.as_object_mut() {
                    if obj.remove(LOCATE_ANCHOR_TAG).is_some() {
                        entry_index = Some(index);
                    }
                }
            }
            let locate = match entry_index {
                Some(entry_index) => serde_json::json!({
                    "state": "resolved",
                    "entry_index": entry_index,
                    "total_index": total_index,
                    "anchor": anchor,
                }),
                // The window math guarantees inclusion; a miss is an
                // internal inconsistency and degrades typed, never 500s.
                None => serde_json::json!({
                    "state": "unavailable",
                    "reason": "located entry fell outside the served page",
                }),
            };
            (page, locate)
        }
        LocateOutcome::Resolved { .. } => (
            // Defensive: resolution derived an index past the entry list.
            session_detail_page_entries(entries, effective_limit, before),
            serde_json::json!({
                "state": "unavailable",
                "reason": "located entry fell outside the transcript",
            }),
        ),
        LocateOutcome::Stale { reason } => (
            session_detail_page_entries(entries, effective_limit, before),
            serde_json::json!({ "state": "stale", "reason": reason }),
        ),
        LocateOutcome::Unavailable { reason } => (
            session_detail_page_entries(entries, effective_limit, before),
            serde_json::json!({ "state": "unavailable", "reason": reason }),
        ),
    }
}

// ---------------------------------------------------------------------
// Native (intendant) resolution
// ---------------------------------------------------------------------

/// 1-based, JSON-parsed `session.jsonl` lines — the same numbering the
/// extractors mint locators in (physical lines; torn tails simply fail
/// the parse).
fn parsed_session_lines(contents: &str) -> impl Iterator<Item = (u64, serde_json::Value)> + '_ {
    contents.lines().enumerate().filter_map(|(index, line)| {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        serde_json::from_str::<serde_json::Value>(trimmed)
            .ok()
            .map(|value| (index as u64 + 1, value))
    })
}

/// The user-side texts a legacy `NativeEvent` locator can hash: the
/// extractor mints them from `session_started` tasks, delivered steers
/// (anchored on the requested line), and `"Round {N} follow-up:"` info
/// lines — verification re-derives the same candidates.
fn native_event_line_texts(event: &serde_json::Value) -> Vec<String> {
    let data = event.get("data");
    match event.get("event").and_then(|value| value.as_str()) {
        Some("session_started") => data
            .and_then(|data| data.get("task"))
            .and_then(|value| value.as_str())
            .map(|task| vec![task.to_string()])
            .unwrap_or_default(),
        Some("steer_requested") => data
            .and_then(|data| data.get("text"))
            .and_then(|value| value.as_str())
            .map(|text| vec![text.to_string()])
            .unwrap_or_default(),
        Some("info") => event
            .get("message")
            .and_then(|value| value.as_str())
            .and_then(parse_round_follow_up)
            .map(|text| vec![text])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Read a sidecar span exactly the way the extractor does (same trust
/// boundary, same bounded length), so the verification hash can only
/// agree or disagree on identical bytes.
fn locate_sidecar_span_text(
    log_dir: &Path,
    file: &str,
    offset: u64,
    len: u64,
) -> Result<String, LocateOutcome> {
    let relative = Path::new(file);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(LocateOutcome::unavailable(
            "sidecar reference escapes the session directory",
        ));
    }
    let path = log_dir.join(relative);
    let Ok(mut handle) = std::fs::File::open(&path) else {
        return Err(LocateOutcome::unavailable(format!(
            "sidecar file missing: {file}"
        )));
    };
    let bounded = len.min(MESSAGE_TEXT_CAP_BYTES as u64 + 8);
    let mut spanned = vec![0u8; bounded as usize];
    if handle.seek(SeekFrom::Start(offset)).is_err() || handle.read_exact(&mut spanned).is_err() {
        return Err(LocateOutcome::stale(
            "sidecar span is out of range — the sidecar file changed",
        ));
    }
    Ok(String::from_utf8_lossy(&spanned).into_owned())
}

/// Map a verified source line to an entry index: exact when the line
/// renders an entry, else the nearest rendered neighbor (preceding
/// preferred — diagnostic twins precede their canonical rows).
fn anchor_for_line(entry_lines: &[Option<u64>], line: u64) -> LocateOutcome {
    if let Some(index) = entry_lines.iter().position(|l| *l == Some(line)) {
        return LocateOutcome::Resolved {
            total_index: index,
            anchor: "exact",
        };
    }
    let mut best_before: Option<usize> = None;
    let mut best_after: Option<usize> = None;
    for (index, entry_line) in entry_lines.iter().enumerate() {
        let Some(entry_line) = entry_line else {
            continue;
        };
        if *entry_line <= line {
            // Lines are non-decreasing along the entry list, so the last
            // write is the greatest line ≤ the target.
            best_before = Some(index);
        } else if best_after.is_none() {
            best_after = Some(index);
        }
    }
    match best_before.or(best_after) {
        Some(index) => LocateOutcome::Resolved {
            total_index: index,
            anchor: "nearest",
        },
        // Only synthetic prelude entries exist: anchor at the top.
        None if !entry_lines.is_empty() => LocateOutcome::Resolved {
            total_index: 0,
            anchor: "nearest",
        },
        None => LocateOutcome::unavailable("the session renders no entries to anchor on"),
    }
}

fn native_locator_outcome(
    log_dir: &Path,
    contents: &str,
    entry_lines: &[Option<u64>],
    locator: &Locator,
) -> LocateOutcome {
    match locator {
        Locator::NativeMessageId { message_id } => {
            for (line_no, event) in parsed_session_lines(contents) {
                if event.get("event").and_then(|value| value.as_str())
                    != Some("conversation_message")
                {
                    continue;
                }
                if event
                    .pointer("/data/message_id")
                    .and_then(|value| value.as_str())
                    == Some(message_id.as_str())
                {
                    return anchor_for_line(entry_lines, line_no);
                }
            }
            LocateOutcome::unavailable("message not found in the session log")
        }
        // line_no 0 = "no event line; sourced from session_meta.json"
        // (the legacy top-level task fallback).
        Locator::NativeEvent {
            line_no: 0,
            content_hash16,
        } => {
            let task = std::fs::read_to_string(log_dir.join("session_meta.json"))
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .and_then(|meta| {
                    meta.get("task")
                        .and_then(|value| value.as_str())
                        .map(str::to_string)
                });
            let Some(task) = task else {
                return LocateOutcome::unavailable(
                    "session metadata no longer carries the task",
                );
            };
            if content_hash_hex16(&task) == *content_hash16 {
                // Meta-sourced records have no event line: the top of the
                // log is the honest anchor.
                LocateOutcome::Resolved {
                    total_index: 0,
                    anchor: "nearest",
                }
            } else {
                LocateOutcome::stale("the session task text changed")
            }
        }
        Locator::NativeEvent {
            line_no,
            content_hash16,
        } => {
            let Some(line) = contents.lines().nth(*line_no as usize - 1) else {
                return LocateOutcome::stale(format!(
                    "the session log no longer reaches line {line_no}"
                ));
            };
            let Ok(event) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                return LocateOutcome::stale(format!(
                    "line {line_no} is no longer a session event"
                ));
            };
            let texts = native_event_line_texts(&event);
            if texts.is_empty() {
                return LocateOutcome::stale(format!(
                    "line {line_no} no longer carries a user message"
                ));
            }
            if texts
                .iter()
                .any(|text| content_hash_hex16(text) == *content_hash16)
            {
                anchor_for_line(entry_lines, *line_no)
            } else {
                LocateOutcome::stale(format!("the message at line {line_no} changed"))
            }
        }
        Locator::NativeSidecarSpan {
            file,
            offset,
            len,
            content_hash16,
        } => {
            let text = match locate_sidecar_span_text(log_dir, file, *offset, *len) {
                Ok(text) => text,
                Err(outcome) => return outcome,
            };
            if content_hash_hex16(&text) != *content_hash16 {
                return LocateOutcome::stale(
                    "the sidecar span content changed — the response was rewritten",
                );
            }
            // Anchor on the event referencing the span. The diagnostic
            // `model_response` renders an entry; the canonical
            // `conversation_message` (same span) does not — prefer
            // whichever line maps exactly.
            let referencing: Vec<u64> = parsed_session_lines(contents)
                .filter(|(_, event)| {
                    matches!(
                        event.get("event").and_then(|value| value.as_str()),
                        Some("model_response") | Some("conversation_message")
                    ) && event.get("file").and_then(|value| value.as_str())
                        == Some(file.as_str())
                        && event
                            .pointer("/data/model_offset")
                            .and_then(|value| value.as_u64())
                            == Some(*offset)
                })
                .map(|(line_no, _)| line_no)
                .collect();
            for line_no in &referencing {
                if entry_lines.contains(&Some(*line_no)) {
                    return anchor_for_line(entry_lines, *line_no);
                }
            }
            match referencing.first() {
                Some(line_no) => anchor_for_line(entry_lines, *line_no),
                None => {
                    LocateOutcome::unavailable("no session event references the sidecar span")
                }
            }
        }
        Locator::ExternalRecordId { .. } | Locator::ExternalLine { .. } => {
            LocateOutcome::unavailable(
                "locator kind does not match the requested source (external locator, native session)",
            )
        }
    }
}

// ---------------------------------------------------------------------
// External (Codex / Claude Code) resolution
// ---------------------------------------------------------------------

/// The transcript file the detail view renders — the same dispatch as
/// `external_session_entries_from_home`.
fn external_transcript_path(home: &Path, source: &str, session_id: &str) -> Option<PathBuf> {
    match source {
        "codex" => find_codex_session_file(home, session_id),
        "claude-code" => find_claude_session_file_for_transcript(home, session_id),
        "gemini" => find_gemini_session_file_for_transcript(home, session_id),
        _ => None,
    }
}

/// Texts an external record can hash — Codex event/payload lanes (the
/// shapes `ExternalLine` locators are minted from) plus the Claude
/// message-content shapes for forward-compat.
fn external_line_texts(obj: &serde_json::Value) -> Vec<String> {
    let mut texts = Vec::new();
    if let Some(payload) = obj.get("payload") {
        if let Some((_, text)) = codex_event_message_text(payload) {
            texts.push(text);
        }
        if let Some((_, text)) = codex_payload_text(payload) {
            texts.push(text);
        }
    }
    if let Some(content) = obj
        .get("message")
        .and_then(|message| message.get("content"))
    {
        texts.extend(message_prose_text(content));
        texts.extend(message_content_text(content));
    }
    texts
}

/// Last entry at or before `ts` (entries are in transcript order and both
/// sides are uniform RFC3339/ISO-8601 UTC strings, so lexicographic
/// comparison orders correctly), else the first entry.
fn nearest_entry_by_ts(entries: &[serde_json::Value], ts: &str) -> Option<usize> {
    if entries.is_empty() {
        return None;
    }
    if ts.is_empty() {
        return Some(0);
    }
    let mut best: Option<usize> = None;
    for (index, entry) in entries.iter().enumerate() {
        let Some(entry_ts) = entry
            .get("ts")
            .and_then(|value| value.as_str())
            .filter(|entry_ts| !entry_ts.is_empty())
        else {
            continue;
        };
        if entry_ts <= ts {
            best = Some(index);
        }
    }
    best.or(Some(0))
}

fn nearest_entry_outcome(entries: &[serde_json::Value], ts: &str) -> LocateOutcome {
    match nearest_entry_by_ts(entries, ts) {
        Some(index) => LocateOutcome::Resolved {
            total_index: index,
            anchor: "nearest",
        },
        None => LocateOutcome::unavailable("the transcript renders no entries to anchor on"),
    }
}

/// Claude: find the record with this `uuid` in the main transcript and
/// anchor its rendered entry (entries carry no ids — match the same
/// `(timestamp, content)` the entry builder used). A uuid that lives only
/// in a subagent transcript is typed `unavailable` with the reason: the
/// detail view renders the main transcript only.
fn claude_record_outcome(
    entries: &[serde_json::Value],
    path: &Path,
    record_id: &str,
) -> LocateOutcome {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return LocateOutcome::unavailable("session transcript unreadable");
    };
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.contains(record_id) {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if obj.get("uuid").and_then(|value| value.as_str()) != Some(record_id) {
            continue;
        }
        let ts = value_str(&obj, "timestamp").unwrap_or_default();
        let texts: Vec<String> = obj
            .get("message")
            .and_then(|message| message.get("content"))
            .map(|content| {
                [message_prose_text(content), message_content_text(content)]
                    .into_iter()
                    .flatten()
                    .collect()
            })
            .unwrap_or_default();
        if let Some(index) = entries.iter().position(|entry| {
            entry.get("ts").and_then(|value| value.as_str()) == Some(ts.as_str())
                && entry
                    .get("content")
                    .and_then(|value| value.as_str())
                    .is_some_and(|content| texts.iter().any(|text| text == content))
        }) {
            return LocateOutcome::Resolved {
                total_index: index,
                anchor: "exact",
            };
        }
        // The record exists but renders no entry (filtered lane): anchor
        // the moment instead.
        return nearest_entry_outcome(entries, &ts);
    }
    if claude_record_in_subagent_transcripts(path, record_id) {
        return LocateOutcome::unavailable(
            "the message is in a subagent transcript, which the session detail view does not display",
        );
    }
    LocateOutcome::unavailable("record not found in the session transcript")
}

/// The subagent transcripts sit beside the main file:
/// `<uuid>/subagents/agent-*.jsonl` (S0b). Strict `.jsonl` extension —
/// `.jsonl.backup`/`.bak*` siblings are excluded.
fn claude_record_in_subagent_transcripts(main_path: &Path, record_id: &str) -> bool {
    let Some(stem) = main_path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let Some(parent) = main_path.parent() else {
        return false;
    };
    let Ok(dir) = std::fs::read_dir(parent.join(stem).join("subagents")) else {
        return false;
    };
    for entry in dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in contents.lines() {
            if !line.contains(record_id) {
                continue;
            }
            let Ok(obj) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            if obj.get("uuid").and_then(|value| value.as_str()) == Some(record_id) {
                return true;
            }
        }
    }
    false
}

/// Codex: the exact `item_id` entry match already failed (caller); find
/// the response_item in the rollout to at least anchor its moment. A
/// missing id in the current rollout usually means a same-thread restore
/// rewrote the file.
fn codex_record_outcome(
    entries: &[serde_json::Value],
    path: &Path,
    record_id: &str,
) -> LocateOutcome {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return LocateOutcome::unavailable("session transcript unreadable");
    };
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.contains(record_id) {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        if codex_response_item_id(&obj).as_deref() != Some(record_id) {
            continue;
        }
        let ts = value_str(&obj, "timestamp").unwrap_or_default();
        return nearest_entry_outcome(entries, &ts);
    }
    LocateOutcome::unavailable(
        "record not found in the current rollout — the thread may have been rewritten",
    )
}

fn external_locator_outcome(
    home: &Path,
    source: &str,
    session_id: &str,
    entries: &[serde_json::Value],
    locator: &Locator,
) -> LocateOutcome {
    match locator {
        Locator::ExternalRecordId { record_id } => {
            // Codex message entries carry the real response_item id.
            if let Some(index) = entries.iter().position(|entry| {
                entry.get("item_id").and_then(|value| value.as_str()) == Some(record_id.as_str())
            }) {
                return LocateOutcome::Resolved {
                    total_index: index,
                    anchor: "exact",
                };
            }
            let Some(path) = external_transcript_path(home, source, session_id) else {
                return LocateOutcome::unavailable("session transcript not found");
            };
            match source {
                "claude-code" => claude_record_outcome(entries, &path, record_id),
                "codex" => codex_record_outcome(entries, &path, record_id),
                other => LocateOutcome::unavailable(format!(
                    "locate is not supported for {other} sessions"
                )),
            }
        }
        Locator::ExternalLine {
            line_no,
            content_hash16,
            // Best-effort by design (plan §7): the generation pinned the
            // rewrite that minted the locator, but only the live file is
            // resolvable — the content hash is the actual verifier.
            generation: _,
        } => {
            let Some(path) = external_transcript_path(home, source, session_id) else {
                return LocateOutcome::unavailable("session transcript not found");
            };
            let Ok(contents) = std::fs::read_to_string(&path) else {
                return LocateOutcome::unavailable("session transcript unreadable");
            };
            if *line_no == 0 {
                return LocateOutcome::stale("locator points at no transcript line");
            }
            let Some(line) = contents.lines().nth(*line_no as usize - 1) else {
                return LocateOutcome::stale(format!(
                    "the transcript no longer reaches line {line_no} — the thread was rewritten"
                ));
            };
            let Ok(obj) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                return LocateOutcome::stale(format!(
                    "line {line_no} is no longer a transcript record"
                ));
            };
            let ts = value_str(&obj, "timestamp").unwrap_or_default();
            let matched = external_line_texts(&obj)
                .into_iter()
                .find(|text| content_hash_hex16(text) == *content_hash16);
            let Some(text) = matched else {
                return LocateOutcome::stale(format!(
                    "the message at line {line_no} changed — the thread was rewritten"
                ));
            };
            if let Some(index) = entries.iter().position(|entry| {
                entry.get("content").and_then(|value| value.as_str()) == Some(text.as_str())
                    && (ts.is_empty()
                        || entry.get("ts").and_then(|value| value.as_str()) == Some(ts.as_str()))
            }) {
                return LocateOutcome::Resolved {
                    total_index: index,
                    anchor: "exact",
                };
            }
            nearest_entry_outcome(entries, &ts)
        }
        Locator::NativeMessageId { .. }
        | Locator::NativeEvent { .. }
        | Locator::NativeSidecarSpan { .. } => LocateOutcome::unavailable(
            "locator kind does not match the requested source (native locator, external session)",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_json(body: &str) -> serde_json::Value {
        serde_json::from_str(body).expect("detail body is JSON")
    }

    fn locator_of(record: &serde_json::Value) -> String {
        record.to_string()
    }

    // ------------------------------------------------------------------
    // Parameter decode
    // ------------------------------------------------------------------

    #[test]
    fn locate_param_accepts_raw_json_and_base64url() {
        let json = r#"{"kind":"native_message_id","message_id":"mid-1"}"#;
        assert_eq!(
            parse_locate_param(json).unwrap(),
            Locator::NativeMessageId {
                message_id: "mid-1".into()
            }
        );
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json);
        assert_eq!(
            parse_locate_param(&encoded).unwrap(),
            Locator::NativeMessageId {
                message_id: "mid-1".into()
            }
        );
        // Padded base64url decodes too.
        let padded = base64::engine::general_purpose::URL_SAFE.encode(json);
        assert_eq!(
            parse_locate_param(&padded).unwrap(),
            Locator::NativeMessageId {
                message_id: "mid-1".into()
            }
        );
    }

    #[test]
    fn locate_param_rejects_garbage_unknown_kinds_and_oversize() {
        assert!(parse_locate_param("").is_err());
        assert!(parse_locate_param("###not-base64###").is_err());
        assert!(parse_locate_param(r#"{"kind":"warp_drive","q":1}"#).is_err());
        assert!(parse_locate_param(r#"{"message_id":"untagged"}"#).is_err());
        let oversize = format!(
            "{{\"kind\":\"native_message_id\",\"message_id\":\"{}\"}}",
            "x".repeat(LOCATE_PARAM_MAX_BYTES)
        );
        assert!(parse_locate_param(&oversize).is_err());
    }

    // ------------------------------------------------------------------
    // Native lane fixtures
    // ------------------------------------------------------------------

    const NATIVE_SESSION: &str = "locate-native-session";

    /// A new-era session log mirroring the extractor fixtures: legacy
    /// diagnostics (session_started, follow-up info, model_response with
    /// a sidecar span) plus canonical conversation_message rows.
    fn write_native_fixture(home: &Path) -> std::path::PathBuf {
        let log_dir = home.join(".intendant").join("logs").join(NATIVE_SESSION);
        std::fs::create_dir_all(log_dir.join("turns")).unwrap();
        std::fs::write(
            log_dir.join("turns").join("turn_001_model.txt"),
            "HELLOFULL ASSISTANT TEXT",
        )
        .unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": NATIVE_SESSION,
                "created_at": "2026-07-01T10:00:00",
                "created_at_ms": 1_780_000_000_000_i64,
                "task": "build the thing",
            })
            .to_string(),
        )
        .unwrap();
        let lines = [
            serde_json::json!({"ts":"10:00:00.000","ts_ms":1,"event":"session_started",
                "data":{"session_id":NATIVE_SESSION,"task":"build the thing"}}),
            serde_json::json!({"ts":"10:00:00.500","ts_ms":2,"event":"info",
                "message":"Round 2 follow-up: also this"}),
            serde_json::json!({"ts":"10:00:01.000","ts_ms":3,"event":"model_response",
                "message":"FULL ASSISTANT TEXT",
                "file":"turns/turn_001_model.txt",
                "data":{"model_offset":5,"model_bytes":19,"content_length":19,
                        "tokens":{"prompt":1,"completion":1,"total":2,"cached":0,"cache_creation":0}}}),
            serde_json::json!({"ts":"10:00:02.000","ts_ms":1_000,"event":"conversation_message",
                "data":{"message_id":"mid-1","message_seq":1,"role":"user",
                         "provenance":"task","text":"build the thing"}}),
            serde_json::json!({"ts":"10:00:03.000","ts_ms":2_000,"event":"conversation_message",
                "file":"turns/turn_001_model.txt",
                "data":{"message_id":"mid-2","message_seq":2,"role":"assistant",
                         "provenance":"assistant","model_offset":5,"model_bytes":19}}),
            serde_json::json!({"ts":"10:00:04.000","ts_ms":3_000,"event":"conversation_message",
                "data":{"message_id":"mid-3","message_seq":3,"role":"user",
                         "provenance":"follow_up","text":"scratch that"}}),
        ];
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(log_dir.join("session.jsonl"), body).unwrap();
        log_dir
    }

    fn native_locate_body(home: &Path, locator: serde_json::Value) -> serde_json::Value {
        let locator = parse_locate_param(&locator_of(&locator)).unwrap();
        body_json(&session_detail_response_body_with_locate(
            home,
            NATIVE_SESSION,
            "intendant",
            None,
            None,
            &locator,
        ))
    }

    fn assert_resolved(body: &serde_json::Value, expect_anchor: &str) -> (usize, usize) {
        let locate = &body["locate"];
        assert_eq!(locate["state"], "resolved", "locate: {locate}");
        assert_eq!(locate["anchor"], expect_anchor, "locate: {locate}");
        let entry_index = locate["entry_index"].as_u64().unwrap() as usize;
        let total_index = locate["total_index"].as_u64().unwrap() as usize;
        let entries = body["entries"].as_array().unwrap();
        assert!(entry_index < entries.len(), "entry_index within the page");
        let page_start = body["page_start"].as_u64().unwrap() as usize;
        let page_end = body["page_end"].as_u64().unwrap() as usize;
        assert!(
            (page_start..page_end).contains(&total_index),
            "total_index {total_index} within [{page_start}, {page_end})"
        );
        (entry_index, total_index)
    }

    // ------------------------------------------------------------------
    // NativeMessageId
    // ------------------------------------------------------------------

    #[test]
    fn native_message_id_resolves_assistant_to_its_model_response_twin() {
        let home = tempfile::tempdir().unwrap();
        write_native_fixture(home.path());
        let body = native_locate_body(
            home.path(),
            serde_json::json!({"kind":"native_message_id","message_id":"mid-2"}),
        );
        let (entry_index, _) = assert_resolved(&body, "nearest");
        // The canonical assistant row renders no entry; its diagnostic
        // twin (the model_response right before it) is the anchor.
        assert_eq!(body["entries"][entry_index]["event"], "model_response");
    }

    #[test]
    fn native_message_id_resolves_user_rows_and_misses_unavailable() {
        let home = tempfile::tempdir().unwrap();
        write_native_fixture(home.path());
        let body = native_locate_body(
            home.path(),
            serde_json::json!({"kind":"native_message_id","message_id":"mid-3"}),
        );
        assert_resolved(&body, "nearest");

        let body = native_locate_body(
            home.path(),
            serde_json::json!({"kind":"native_message_id","message_id":"mid-nope"}),
        );
        assert_eq!(body["locate"]["state"], "unavailable");
        assert!(body["locate"]["reason"]
            .as_str()
            .unwrap()
            .contains("not found"));
        // Degraded reads still serve the normal page.
        assert!(body["entries"].as_array().is_some_and(|e| !e.is_empty()));
    }

    // ------------------------------------------------------------------
    // NativeEvent
    // ------------------------------------------------------------------

    #[test]
    fn native_event_verifies_content_hash_and_reports_drift_as_stale() {
        let home = tempfile::tempdir().unwrap();
        write_native_fixture(home.path());
        // The follow-up info line (line 2) renders an entry: exact.
        let body = native_locate_body(
            home.path(),
            serde_json::json!({"kind":"native_event","line_no":2,
                "content_hash16": content_hash_hex16("also this")}),
        );
        assert_resolved(&body, "exact");

        // Same line, different content hash: stale.
        let body = native_locate_body(
            home.path(),
            serde_json::json!({"kind":"native_event","line_no":2,
                "content_hash16": content_hash_hex16("something else")}),
        );
        assert_eq!(body["locate"]["state"], "stale");

        // Beyond the end of the log: stale.
        let body = native_locate_body(
            home.path(),
            serde_json::json!({"kind":"native_event","line_no":99,
                "content_hash16": content_hash_hex16("also this")}),
        );
        assert_eq!(body["locate"]["state"], "stale");
    }

    #[test]
    fn native_event_line_zero_verifies_the_meta_task() {
        let home = tempfile::tempdir().unwrap();
        write_native_fixture(home.path());
        let body = native_locate_body(
            home.path(),
            serde_json::json!({"kind":"native_event","line_no":0,
                "content_hash16": content_hash_hex16("build the thing")}),
        );
        let (_, total_index) = assert_resolved(&body, "nearest");
        assert_eq!(total_index, 0, "meta-sourced records anchor at the top");

        let body = native_locate_body(
            home.path(),
            serde_json::json!({"kind":"native_event","line_no":0,
                "content_hash16": content_hash_hex16("a different task")}),
        );
        assert_eq!(body["locate"]["state"], "stale");
    }

    // ------------------------------------------------------------------
    // NativeSidecarSpan
    // ------------------------------------------------------------------

    #[test]
    fn native_sidecar_span_resolves_to_the_model_response_entry() {
        let home = tempfile::tempdir().unwrap();
        write_native_fixture(home.path());
        let body = native_locate_body(
            home.path(),
            serde_json::json!({"kind":"native_sidecar_span",
                "file":"turns/turn_001_model.txt","offset":5,"len":19,
                "content_hash16": content_hash_hex16("FULL ASSISTANT TEXT")}),
        );
        let (entry_index, _) = assert_resolved(&body, "exact");
        assert_eq!(body["entries"][entry_index]["event"], "model_response");
    }

    #[test]
    fn native_sidecar_span_degrades_typed_on_mismatch_missing_and_escape() {
        let home = tempfile::tempdir().unwrap();
        write_native_fixture(home.path());
        // Hash mismatch (the span bytes say something else): stale.
        let body = native_locate_body(
            home.path(),
            serde_json::json!({"kind":"native_sidecar_span",
                "file":"turns/turn_001_model.txt","offset":0,"len":5,
                "content_hash16": content_hash_hex16("WRONG")}),
        );
        assert_eq!(body["locate"]["state"], "stale");

        // Span past the end of the sidecar: stale.
        let body = native_locate_body(
            home.path(),
            serde_json::json!({"kind":"native_sidecar_span",
                "file":"turns/turn_001_model.txt","offset":1_000,"len":19,
                "content_hash16": content_hash_hex16("FULL ASSISTANT TEXT")}),
        );
        assert_eq!(body["locate"]["state"], "stale");

        // Missing sidecar file: unavailable.
        let body = native_locate_body(
            home.path(),
            serde_json::json!({"kind":"native_sidecar_span",
                "file":"turns/gone.txt","offset":0,"len":4,
                "content_hash16": content_hash_hex16("FULL")}),
        );
        assert_eq!(body["locate"]["state"], "unavailable");

        // Escaping reference: unavailable, and the read never leaves the
        // session dir.
        std::fs::write(home.path().join("outside.txt"), "SECRET").unwrap();
        let body = native_locate_body(
            home.path(),
            serde_json::json!({"kind":"native_sidecar_span",
                "file":"../../../outside.txt","offset":0,"len":6,
                "content_hash16": content_hash_hex16("SECRET")}),
        );
        assert_eq!(body["locate"]["state"], "unavailable");
    }

    // ------------------------------------------------------------------
    // Kind/source mismatch + paging window
    // ------------------------------------------------------------------

    #[test]
    fn locator_kind_source_mismatch_is_unavailable() {
        let home = tempfile::tempdir().unwrap();
        write_native_fixture(home.path());
        let body = native_locate_body(
            home.path(),
            serde_json::json!({"kind":"external_record_id","record_id":"item-1"}),
        );
        assert_eq!(body["locate"]["state"], "unavailable");
        assert!(body["locate"]["reason"]
            .as_str()
            .unwrap()
            .contains("does not match"));
    }

    #[test]
    fn resolved_locate_centers_the_page_on_the_anchor() {
        let home = tempfile::tempdir().unwrap();
        let log_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join(NATIVE_SESSION);
        std::fs::create_dir_all(log_dir.join("turns")).unwrap();
        // Ten spans "response 0".."response 9" concatenated in one sidecar.
        let mut sidecar = String::new();
        let mut spans = Vec::new();
        for index in 0..10 {
            let text = format!("response {index}");
            spans.push((sidecar.len() as u64, text.len() as u64, text.clone()));
            sidecar.push_str(&text);
        }
        std::fs::write(log_dir.join("turns").join("turn_001_model.txt"), &sidecar).unwrap();
        let mut body = String::new();
        for (offset, len, text) in &spans {
            body.push_str(
                &serde_json::json!({"ts":"10:00:01.000","ts_ms":1,"event":"model_response",
                    "message": text,
                    "file":"turns/turn_001_model.txt",
                    "data":{"model_offset":offset,"model_bytes":len,"content_length":len,
                            "tokens":{"prompt":1,"completion":1,"total":2,"cached":0,"cache_creation":0}}})
                .to_string(),
            );
            body.push('\n');
        }
        std::fs::write(log_dir.join("session.jsonl"), body).unwrap();

        // Locate the 5th span (full-list index 5: replay_start + spans 0-4
        // precede it) with a limit of 4.
        let (offset, len, text) = &spans[4];
        let locator = parse_locate_param(
            &serde_json::json!({"kind":"native_sidecar_span",
                "file":"turns/turn_001_model.txt","offset":offset,"len":len,
                "content_hash16": content_hash_hex16(text)})
            .to_string(),
        )
        .unwrap();
        let body = body_json(&session_detail_response_body_with_locate(
            home.path(),
            NATIVE_SESSION,
            "intendant",
            Some(4),
            // An explicit `before` is superseded by the anchor window.
            Some(2),
            &locator,
        ));
        let locate = &body["locate"];
        assert_eq!(locate["state"], "resolved");
        assert_eq!(locate["total_index"], 5);
        // Window: page_end = 5 + 1 + 4/2 = 8, page_start = 4.
        assert_eq!(body["page_start"], 4);
        assert_eq!(body["page_end"], 8);
        // Page = kept replay_start + entries[4..8]; the anchor sits at 1 + (5-4).
        let entry_index = locate["entry_index"].as_u64().unwrap() as usize;
        assert_eq!(entry_index, 2);
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0]["event"], "replay_start");
        assert_eq!(entries[entry_index]["summary"], "response 4");
    }

    // ------------------------------------------------------------------
    // Codex lane
    // ------------------------------------------------------------------

    const CODEX_SESSION: &str = "019e37ae-f523-73b0-8bb4-locate000001";

    fn write_codex_fixture(home: &Path) {
        let sessions_dir = home
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("07")
            .join("01");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let lines = [
            serde_json::json!({"timestamp":"2026-07-01T10:00:00.000Z","type":"session_meta",
                "payload":{"id":CODEX_SESSION,"timestamp":"2026-07-01T10:00:00.000Z","cwd":"/repo"}}),
            serde_json::json!({"timestamp":"2026-07-01T10:00:01.000Z","type":"event_msg",
                "payload":{"type":"user_message","message":"find the bug"}}),
            serde_json::json!({"timestamp":"2026-07-01T10:00:01.500Z","type":"response_item",
                "payload":{"type":"message","role":"user",
                            "content":[{"type":"input_text","text":"find the bug"}]}}),
            serde_json::json!({"timestamp":"2026-07-01T10:00:02.000Z","type":"response_item",
                "payload":{"type":"message","role":"assistant","id":"msg_a1",
                            "content":[{"type":"output_text","text":"looking now"}]}}),
        ];
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(
            sessions_dir.join(format!("rollout-2026-07-01T10-00-00-{CODEX_SESSION}.jsonl")),
            body,
        )
        .unwrap();
    }

    fn codex_locate_body(home: &Path, locator: serde_json::Value) -> serde_json::Value {
        let locator = parse_locate_param(&locator_of(&locator)).unwrap();
        body_json(&session_detail_response_body_with_locate(
            home,
            CODEX_SESSION,
            "codex",
            None,
            None,
            &locator,
        ))
    }

    #[test]
    fn codex_external_record_id_resolves_by_item_id() {
        let home = tempfile::tempdir().unwrap();
        write_codex_fixture(home.path());
        let body = codex_locate_body(
            home.path(),
            serde_json::json!({"kind":"external_record_id","record_id":"msg_a1"}),
        );
        let (entry_index, _) = assert_resolved(&body, "exact");
        assert_eq!(body["entries"][entry_index]["content"], "looking now");
        assert_eq!(body["entries"][entry_index]["item_id"], "msg_a1");

        let body = codex_locate_body(
            home.path(),
            serde_json::json!({"kind":"external_record_id","record_id":"msg_gone"}),
        );
        assert_eq!(body["locate"]["state"], "unavailable");
        assert!(body["locate"]["reason"]
            .as_str()
            .unwrap()
            .contains("rewritten"));
    }

    #[test]
    fn codex_external_line_verifies_the_live_line() {
        let home = tempfile::tempdir().unwrap();
        write_codex_fixture(home.path());
        // Line 2 is the event_msg user_message ("find the bug").
        let body = codex_locate_body(
            home.path(),
            serde_json::json!({"kind":"external_line","generation":0,"line_no":2,
                "content_hash16": content_hash_hex16("find the bug")}),
        );
        let (entry_index, _) = assert_resolved(&body, "exact");
        assert_eq!(body["entries"][entry_index]["content"], "find the bug");

        // A rewritten thread: the hash no longer matches the line.
        let body = codex_locate_body(
            home.path(),
            serde_json::json!({"kind":"external_line","generation":0,"line_no":2,
                "content_hash16": content_hash_hex16("previous generation text")}),
        );
        assert_eq!(body["locate"]["state"], "stale");
        assert!(body["locate"]["reason"]
            .as_str()
            .unwrap()
            .contains("rewritten"));

        // A shortened (rewritten) rollout: the line is gone.
        let body = codex_locate_body(
            home.path(),
            serde_json::json!({"kind":"external_line","generation":1,"line_no":40,
                "content_hash16": content_hash_hex16("find the bug")}),
        );
        assert_eq!(body["locate"]["state"], "stale");
    }

    // ------------------------------------------------------------------
    // Claude lane
    // ------------------------------------------------------------------

    const CLAUDE_SESSION: &str = "6f0deca6-a7cc-4d05-a96c-locate000002";

    fn write_claude_fixture(home: &Path) {
        let project_dir = home.join(".claude").join("projects").join("-repo-x");
        std::fs::create_dir_all(&project_dir).unwrap();
        let lines = [
            serde_json::json!({"type":"user","uuid":"u-1","timestamp":"2026-07-10T11:00:00.000Z",
                "sessionId":CLAUDE_SESSION,"isSidechain":false,
                "message":{"role":"user","content":"hello world"}}),
            serde_json::json!({"type":"assistant","uuid":"a-1","timestamp":"2026-07-10T11:00:01.000Z",
                "sessionId":CLAUDE_SESSION,"isSidechain":false,
                "message":{"role":"assistant","content":[{"type":"text","text":"hi there"}]}}),
        ];
        let body: String = lines.iter().map(|line| format!("{line}\n")).collect();
        std::fs::write(project_dir.join(format!("{CLAUDE_SESSION}.jsonl")), body).unwrap();

        // Subagent transcript beside the main file (S0b layout).
        let subagents = project_dir.join(CLAUDE_SESSION).join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(
            subagents.join("agent-abc123.jsonl"),
            serde_json::json!({"type":"assistant","uuid":"sub-1",
                "timestamp":"2026-07-10T11:00:02.000Z","sessionId":CLAUDE_SESSION,
                "isSidechain":true,"agentId":"abc123",
                "message":{"role":"assistant","content":[{"type":"text","text":"subagent prose"}]}})
            .to_string()
                + "\n",
        )
        .unwrap();
    }

    fn claude_locate_body(home: &Path, locator: serde_json::Value) -> serde_json::Value {
        let locator = parse_locate_param(&locator_of(&locator)).unwrap();
        body_json(&session_detail_response_body_with_locate(
            home,
            CLAUDE_SESSION,
            "claude-code",
            None,
            None,
            &locator,
        ))
    }

    #[test]
    fn claude_external_record_id_resolves_by_uuid_content_match() {
        let home = tempfile::tempdir().unwrap();
        write_claude_fixture(home.path());
        let body = claude_locate_body(
            home.path(),
            serde_json::json!({"kind":"external_record_id","record_id":"a-1"}),
        );
        let (entry_index, _) = assert_resolved(&body, "exact");
        assert_eq!(body["entries"][entry_index]["content"], "hi there");

        let body = claude_locate_body(
            home.path(),
            serde_json::json!({"kind":"external_record_id","record_id":"u-1"}),
        );
        let (entry_index, _) = assert_resolved(&body, "exact");
        assert_eq!(body["entries"][entry_index]["content"], "hello world");
    }

    #[test]
    fn claude_subagent_and_missing_records_are_unavailable() {
        let home = tempfile::tempdir().unwrap();
        write_claude_fixture(home.path());
        let body = claude_locate_body(
            home.path(),
            serde_json::json!({"kind":"external_record_id","record_id":"sub-1"}),
        );
        assert_eq!(body["locate"]["state"], "unavailable");
        assert!(body["locate"]["reason"]
            .as_str()
            .unwrap()
            .contains("subagent"));

        let body = claude_locate_body(
            home.path(),
            serde_json::json!({"kind":"external_record_id","record_id":"nowhere-1"}),
        );
        assert_eq!(body["locate"]["state"], "unavailable");
        assert!(body["locate"]["reason"]
            .as_str()
            .unwrap()
            .contains("not found"));
    }

    // ------------------------------------------------------------------
    // Missing session
    // ------------------------------------------------------------------

    #[test]
    fn missing_sessions_keep_the_historical_error_body() {
        let home = tempfile::tempdir().unwrap();
        let locator =
            parse_locate_param(r#"{"kind":"native_message_id","message_id":"mid-1"}"#).unwrap();
        let body = body_json(&session_detail_response_body_with_locate(
            home.path(),
            "no-such-session",
            "intendant",
            None,
            None,
            &locator,
        ));
        assert_eq!(body["error"], "session not found");

        let locator =
            parse_locate_param(r#"{"kind":"external_record_id","record_id":"x"}"#).unwrap();
        let body = body_json(&session_detail_response_body_with_locate(
            home.path(),
            "no-such-session",
            "codex",
            None,
            None,
            &locator,
        ));
        assert_eq!(body["error"], "session not found");
    }
}
