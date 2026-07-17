//! Mid-turn steer ledger: the wrapper session log's record of which steer
//! texts entered the backend conversation INSIDE an already-running turn.
//!
//! Why it exists: a mid-turn steer (Codex `turn/steer`) puts a plain user
//! message into the backend's own transcript without incrementing the
//! wrapper's round counter — the live lane logs it via
//! `emit_user_message_log` with NO `user_turn_index` (see the accept path
//! in `external_events.rs`). Blind positional counting in the transcript
//! parsers therefore drifts after every mid-turn steer: hydrated user rows
//! claim indexes the live lane never emitted, post-steer follow-ups miss
//! the text-signature dedupe bridge, and edit/rewind affordances target
//! the wrong turn. The parsers consult this ledger to classify a
//! transcript user row as a mid-turn steer (rendered WITHOUT turn
//! metadata, exactly like its live row) instead of burning a turn index
//! on it.
//!
//! Ground truth is the wrapper's own `session.jsonl`: `steer_requested`
//! carries the steer text, `steer_accepted` marks the runtime accepting a
//! mid-turn injection (the Codex `turn/steer` OK path), and
//! `steer_delivered` with `mid_turn: true` marks its checkpoint flush.
//! Boundary-drained steers (`mid_turn: false`) never enter the ledger:
//! their text reaches the backend inside a counted follow-up message (the
//! `[User] …` merge in `drain_steer_queue_as_followup`), so positional
//! counting already agrees with the wrapper for them.
//!
//! Safety bar (matches PR #444's): classification may only WITHHOLD turn
//! metadata from a row the live lane also showed without metadata — it
//! never suppresses or merges rows. Each ledger entry is consumed at most
//! once, so a legitimately repeated identical prompt still mints its own
//! turn, and consumption is timestamp-guarded so a transcript row that
//! predates the steer request can never be misclassified.

use super::*;

/// Clock slack for the "a steer's transcript row cannot predate its
/// request" guard. Both timestamps come from the same host (the daemon
/// stamps `steer_requested`, the backend CLI stamps its transcript line)
/// but through different processes and precisions — Codex rollout lines
/// can carry second-precision timestamps, so the guard must tolerate at
/// least one second of rounding. Kept small on purpose: the guard is what
/// makes a cached parse of an unchanged transcript invariant under ledger
/// growth (new entries can only match rows written after the request).
pub(crate) const MID_TURN_STEER_ROW_TS_SLACK_MS: i64 = 2_000;

#[derive(Clone, Debug)]
pub(crate) struct ExternalSteerLedgerEntry {
    /// Trimmed steer text — the same text the live `UserMessageLog` row
    /// carried and the backend recorded as a user message.
    pub(crate) text: String,
    /// `ts_ms` of the `steer_requested` event, when the log carried one.
    pub(crate) requested_ts_ms: Option<i64>,
}

/// Mid-turn steer texts for one external session, in request order.
#[derive(Clone, Debug, Default)]
pub(crate) struct ExternalSteerLedger {
    entries: Vec<ExternalSteerLedgerEntry>,
}

impl ExternalSteerLedger {
    #[cfg(test)]
    pub(crate) fn from_entries(entries: Vec<ExternalSteerLedgerEntry>) -> Self {
        Self { entries }
    }

    pub(crate) fn cursor(&self) -> ExternalSteerCursor<'_> {
        ExternalSteerCursor {
            entries: &self.entries,
            consumed: vec![false; self.entries.len()],
        }
    }
}

/// Per-parse consumption state over a shared ledger: each entry justifies
/// AT MOST one turnless user row, so identical repeated prompts keep
/// minting distinct turns once the matching steer entries are spent.
pub(crate) struct ExternalSteerCursor<'a> {
    entries: &'a [ExternalSteerLedgerEntry],
    consumed: Vec<bool>,
}

impl ExternalSteerCursor<'_> {
    /// True iff `text` matches an unconsumed mid-turn steer entry whose
    /// request does not postdate the row (within clock slack); consumes
    /// the matched entry. Rows without a parseable timestamp are never
    /// classified as steers (fail-closed: a phantom turn index is the
    /// known pre-existing failure mode; misclassifying an unrelated row
    /// would be a new one).
    pub(crate) fn try_consume_mid_turn_steer(
        &mut self,
        text: &str,
        row_ts_ms: Option<i64>,
    ) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return false;
        }
        let Some(row_ts_ms) = row_ts_ms else {
            return false;
        };
        for (index, entry) in self.entries.iter().enumerate() {
            if self.consumed[index] || entry.text != text {
                continue;
            }
            if let Some(requested_ts_ms) = entry.requested_ts_ms {
                if row_ts_ms.saturating_add(MID_TURN_STEER_ROW_TS_SLACK_MS) < requested_ts_ms {
                    continue;
                }
            }
            self.consumed[index] = true;
            return true;
        }
        false
    }
}

/// Wrapper log dirs consulted for the ledger: the managed-context named
/// dir, the wrapper-index records, and the session-list cache's observed
/// dirs. Deliberately NOT the recent-log content scan that
/// `external_context_snapshot_replay_log_dirs` falls back to — a session
/// with no known wrapper has no live turn lane to align with (foreign or
/// imported transcripts), and the content scan reads whole logs per fetch.
/// Returns each dir with the wrapper session ids that may label its steer
/// events (`data.session_id` is the LIVE session address at request time,
/// which can be the wrapper id before the native backend id is announced).
fn external_steer_ledger_log_dirs(
    home: &Path,
    source: &str,
    session_id: &str,
) -> Vec<(PathBuf, Vec<String>)> {
    let mut dirs: Vec<(PathBuf, Vec<String>)> = Vec::new();
    let mut seen_dirs: HashSet<String> = HashSet::new();
    let mut push_dir = |dirs: &mut Vec<(PathBuf, Vec<String>)>, path: PathBuf, alias: Option<String>| {
        let key = std::fs::canonicalize(&path)
            .unwrap_or_else(|_| path.clone())
            .to_string_lossy()
            .to_string();
        let dir_name_alias = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string);
        if seen_dirs.insert(key) {
            let mut aliases = Vec::new();
            aliases.extend(dir_name_alias);
            aliases.extend(alias);
            dirs.push((path, aliases));
        } else if let Some(alias) = alias {
            if let Some((_, aliases)) = dirs.iter_mut().find(|(existing, _)| *existing == path) {
                if !aliases.contains(&alias) {
                    aliases.push(alias);
                }
            }
        }
    };

    if let Some(path) = managed_context_named_log_dir(home, session_id) {
        push_dir(&mut dirs, path, None);
    }
    for record in crate::external_wrapper_index::wrappers_for(home, source, session_id) {
        let alias = Some(record.intendant_session_id.clone()).filter(|id| !id.trim().is_empty());
        push_dir(&mut dirs, PathBuf::from(record.log_path), alias);
    }
    for path in cached_intendant_log_dirs_for_session_id(session_id) {
        push_dir(&mut dirs, path, None);
    }

    dirs
}

/// Build the mid-turn steer ledger for `(source, session_id)` from its
/// wrapper session logs. Requested texts join accepted/mid-turn-delivered
/// ids across all discovered logs (a resumed session's request and
/// delivery can land in different wrapper epochs).
pub(crate) fn external_mid_turn_steer_ledger(
    home: &Path,
    source: &str,
    session_id: &str,
) -> ExternalSteerLedger {
    struct RequestedSteer {
        text: String,
        ts_ms: Option<i64>,
        order: usize,
    }
    let mut requested: HashMap<String, RequestedSteer> = HashMap::new();
    let mut injected_ids: HashSet<String> = HashSet::new();
    let mut order = 0usize;

    for (dir, aliases) in external_steer_ledger_log_dirs(home, source, session_id) {
        let Ok(file) = std::fs::File::open(dir.join("session.jsonl")) else {
            continue;
        };
        let reader = std::io::BufReader::new(file);
        for line in std::io::BufRead::lines(reader) {
            let Ok(line) = line else {
                continue;
            };
            // Substring prefilter keeps the JSON parse off the (vastly
            // dominant) non-steer lines.
            if !line.contains("steer_") {
                continue;
            }
            let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let event_kind = event.get("event").and_then(|v| v.as_str()).unwrap_or("");
            if !matches!(
                event_kind,
                "steer_requested" | "steer_accepted" | "steer_delivered"
            ) {
                continue;
            }
            let data = event.get("data");
            // Steers for OTHER sessions sharing this wrapper log (side
            // threads, Codex subagents) must not leak into this
            // transcript's ledger; an absent session id means the
            // wrapper's primary session — this one.
            let event_session_id = data
                .and_then(|d| d.get("session_id"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .unwrap_or("");
            if !event_session_id.is_empty()
                && event_session_id != session_id
                && !aliases.iter().any(|alias| alias == event_session_id)
            {
                continue;
            }
            let Some(id) = data
                .and_then(|d| d.get("id"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            match event_kind {
                "steer_requested" => {
                    let Some(text) = data
                        .and_then(|d| d.get("text"))
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                    else {
                        continue;
                    };
                    requested.entry(id.to_string()).or_insert_with(|| {
                        order += 1;
                        RequestedSteer {
                            text: text.to_string(),
                            ts_ms: event.get("ts_ms").and_then(|v| v.as_i64()),
                            order,
                        }
                    });
                }
                "steer_accepted" => {
                    injected_ids.insert(id.to_string());
                }
                "steer_delivered" => {
                    if data
                        .and_then(|d| d.get("mid_turn"))
                        .and_then(|v| v.as_bool())
                        == Some(true)
                    {
                        injected_ids.insert(id.to_string());
                    }
                }
                _ => {}
            }
        }
    }

    let mut matched: Vec<RequestedSteer> = requested
        .into_iter()
        .filter_map(|(id, entry)| injected_ids.contains(&id).then_some(entry))
        .collect();
    matched.sort_by_key(|entry| (entry.ts_ms.unwrap_or(i64::MAX), entry.order));
    ExternalSteerLedger {
        entries: matched
            .into_iter()
            .map(|entry| ExternalSteerLedgerEntry {
                text: entry.text,
                requested_ts_ms: entry.ts_ms,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steer_line(
        event: &str,
        ts_ms: i64,
        id: &str,
        session_id: Option<&str>,
        text: Option<&str>,
        mid_turn: Option<bool>,
    ) -> String {
        let mut data = serde_json::json!({ "id": id, "status": "x" });
        if let Some(session_id) = session_id {
            data["session_id"] = serde_json::json!(session_id);
        }
        if let Some(text) = text {
            data["text"] = serde_json::json!(text);
        }
        if let Some(mid_turn) = mid_turn {
            data["mid_turn"] = serde_json::json!(mid_turn);
        }
        serde_json::json!({
            "ts": "12:00:00",
            "ts_ms": ts_ms,
            "event": event,
            "level": "info",
            "message": format!("Steer {event}"),
            "data": data,
        })
        .to_string()
    }

    fn write_wrapper_log(home: &Path, wrapper_id: &str, lines: &[String]) -> PathBuf {
        let dir = home
            .join(".intendant")
            .join("logs")
            .join(wrapper_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session.jsonl"), lines.join("\n")).unwrap();
        dir
    }

    /// Requested+accepted (the Codex `turn/steer` OK arc) and
    /// requested+delivered-mid-turn both enter the ledger; queued
    /// boundary deliveries (`mid_turn: false`) and requested-only steers
    /// do not — their texts reach the backend inside counted follow-up
    /// messages (or never), so classifying them as turnless would
    /// re-create the drift in the other direction.
    #[test]
    fn ledger_admits_only_mid_turn_injected_steers() {
        let home = tempfile::tempdir().unwrap();
        let backend_id = "0199aaaa-ledger-admission";
        let wrapper_id = "wrapper-ledger-admission";
        let dir = write_wrapper_log(
            home.path(),
            wrapper_id,
            &[
                steer_line("steer_requested", 1_000, "s-accepted", None, Some("also do B"), None),
                steer_line("steer_accepted", 1_001, "s-accepted", None, None, None),
                steer_line("steer_requested", 2_000, "s-boundary", None, Some("queued text"), None),
                steer_line("steer_delivered", 2_500, "s-boundary", None, None, Some(false)),
                steer_line("steer_requested", 3_000, "s-pending", None, Some("never landed"), None),
                steer_line("steer_requested", 4_000, "s-checkpoint", None, Some("mid checkpoint"), None),
                steer_line("steer_delivered", 4_500, "s-checkpoint", None, None, Some(true)),
            ],
        );
        crate::external_wrapper_index::upsert(
            home.path(),
            "codex",
            backend_id,
            wrapper_id,
            &dir,
            None,
        )
        .unwrap();

        let ledger = external_mid_turn_steer_ledger(home.path(), "codex", backend_id);
        let texts: Vec<&str> = ledger.entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["also do B", "mid checkpoint"]);
        assert_eq!(ledger.entries[0].requested_ts_ms, Some(1_000));
    }

    /// Steer events labeled with ANOTHER session id (side threads and
    /// Codex subagents share the wrapper log) stay out of this
    /// transcript's ledger, while events labeled with the wrapper's own
    /// intendant session id (the live address before the native backend
    /// id is announced) are admitted.
    #[test]
    fn ledger_filters_other_sessions_but_admits_wrapper_alias() {
        let home = tempfile::tempdir().unwrap();
        let backend_id = "0199aaaa-ledger-alias";
        let wrapper_id = "wrapper-ledger-alias";
        let dir = write_wrapper_log(
            home.path(),
            wrapper_id,
            &[
                steer_line(
                    "steer_requested",
                    1_000,
                    "s-side",
                    Some("side-thread-id"),
                    Some("side steer"),
                    None,
                ),
                steer_line("steer_accepted", 1_001, "s-side", Some("side-thread-id"), None, None),
                steer_line(
                    "steer_requested",
                    2_000,
                    "s-alias",
                    Some(wrapper_id),
                    Some("alias steer"),
                    None,
                ),
                steer_line("steer_accepted", 2_001, "s-alias", Some(wrapper_id), None, None),
                steer_line(
                    "steer_requested",
                    3_000,
                    "s-backend",
                    Some(backend_id),
                    Some("backend steer"),
                    None,
                ),
                steer_line("steer_accepted", 3_001, "s-backend", Some(backend_id), None, None),
            ],
        );
        crate::external_wrapper_index::upsert(
            home.path(),
            "codex",
            backend_id,
            wrapper_id,
            &dir,
            None,
        )
        .unwrap();

        let ledger = external_mid_turn_steer_ledger(home.path(), "codex", backend_id);
        let texts: Vec<&str> = ledger.entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["alias steer", "backend steer"]);
    }

    /// Consumption is one-shot and timestamp-guarded: an entry matches at
    /// most one row, never a row that predates its request (beyond clock
    /// slack), and never a row without a parseable timestamp. The guard is
    /// what keeps a cached parse of an unchanged transcript invariant
    /// under ledger growth.
    #[test]
    fn cursor_consumption_is_one_shot_and_ts_guarded() {
        let ledger = ExternalSteerLedger::from_entries(vec![ExternalSteerLedgerEntry {
            text: "keep going".to_string(),
            requested_ts_ms: Some(10_000),
        }]);
        let mut cursor = ledger.cursor();
        assert!(
            !cursor.try_consume_mid_turn_steer("keep going", Some(10_000 - 5_000)),
            "a row written before the request cannot be its steer"
        );
        assert!(
            !cursor.try_consume_mid_turn_steer("keep going", None),
            "rows without timestamps fail closed (stay counted turns)"
        );
        assert!(
            !cursor.try_consume_mid_turn_steer("different text", Some(11_000)),
            "text must match exactly (trimmed)"
        );
        assert!(cursor.try_consume_mid_turn_steer("  keep going  ", Some(11_000)));
        assert!(
            !cursor.try_consume_mid_turn_steer("keep going", Some(12_000)),
            "an entry justifies at most one turnless row — the repeated \
             identical prompt after it stays a real turn"
        );
    }
}
