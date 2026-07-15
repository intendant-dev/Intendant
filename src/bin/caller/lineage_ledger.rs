use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::UNIX_EPOCH;

/// Boilerplate the session log writes for a `done_signal` with no caller message
/// (see `SessionLog::done_signal_for_session`). Filtered out so it isn't treated
/// as a model-authored branch summary.
const DONE_SIGNAL_DEFAULT_MESSAGE: &str = "Agent signalled done";

/// Parent/child session relationships for one session's connected component,
/// derived from `session.jsonl` (never persisted as its own file). Consumed
/// by the MCP `get_status` surface (`mcp.rs`) and embedded into rewind-record
/// snapshots (`main.rs` / `context_rewind.rs`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineageLedger {
    pub source_session_id: String,
    pub groups: Vec<LineageGroup>,
}

/// All recorded child edges of one parent session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineageGroup {
    pub group_id: String,
    pub parent_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_session_id: Option<String>,
    pub branches: Vec<LineageBranch>,
}

/// One parent→child relationship row (see [`lineage_ledger_from_jsonl`] for
/// the recognized relationship kinds and their status conventions).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineageBranch {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_session_id: Option<String>,
    pub relationship: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub raw_log: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ephemeral: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct RelationshipKey {
    parent_session_id: String,
    child_session_id: String,
    relationship: String,
    ephemeral: bool,
}

#[derive(Default, Clone)]
struct SessionFacts {
    identities: HashMap<String, String>,
    tasks: HashMap<String, String>,
    summaries: HashMap<String, String>,
    statuses: HashMap<String, String>,
    relationships: BTreeSet<RelationshipKey>,
    /// Emission order (sequence index) of each relationship, so canonical-head
    /// selection can pick the *latest* rewind-restore rather than relying on the
    /// `BTreeSet`'s lexicographic ordering of (random) child session ids.
    relationship_order: HashMap<RelationshipKey, usize>,
    /// Next relationship sequence index (fold state — lives here so an
    /// incremental fold continues exactly where the previous one stopped).
    relationship_seq: usize,
    /// `(parent, child)` pairs severed by a `fission-detached` relationship:
    /// the branch's spawn anchor left the effective history (rewound past) or
    /// the group was explicitly severed.
    fission_detached: BTreeSet<(String, String)>,
    /// `(parent, child)` pairs whose result a `fission-imported` relationship
    /// marked as explicitly imported into the parent's continuation.
    fission_imported: BTreeSet<(String, String)>,
    /// Detach/import marker relationships in emission order. Whether a
    /// marker dedups into its spawn row or renders standalone depends on the
    /// full relationship set, so they are resolved at derive time
    /// ([`lineage_ledger_from_facts`]) rather than folded in.
    pending_fission_marks: Vec<RelationshipKey>,
}

/// How many session dirs' folded facts to retain. Facts are small (id/status
/// maps, not the log), but a long-lived daemon touches many session dirs;
/// past the cap the map is cleared and the active dirs re-fold once.
const LINEAGE_FACTS_CACHE_CAP: usize = 64;

/// Folded `session.jsonl` facts plus the cursor they were folded through.
/// `consumed_bytes` only ever advances past consumable content — the writer
/// newline-terminates every event, so a trailing partial line is a
/// mid-flush artifact folded once complete. `(stat_len, mtime_nanos)`
/// record the file stat at the last evaluation (consumed can lag stat_len
/// while a partial tail is pending): an mtime change without growth is a
/// rewrite and re-folds, growth is an append.
struct CachedLineageFacts {
    consumed_bytes: u64,
    stat_len: u64,
    mtime_nanos: u128,
    facts: SessionFacts,
}

fn lineage_facts_cache() -> &'static Mutex<HashMap<PathBuf, Arc<CachedLineageFacts>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<CachedLineageFacts>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn store_lineage_facts(path: &Path, entry: Arc<CachedLineageFacts>) {
    let mut cache = lineage_facts_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if cache.len() >= LINEAGE_FACTS_CACHE_CAP && !cache.contains_key(path) {
        cache.clear();
    }
    cache.insert(path.to_path_buf(), entry);
}

fn mtime_nanos_of(metadata: &fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0)
}

/// Read `session.jsonl` from `log_dir` and derive the lineage ledger for
/// `source_session_id` (see [`lineage_ledger_from_jsonl`]). Called by the MCP
/// `get_status` surface (`mcp.rs`) and the rewind-record snapshot path
/// (`main.rs`). `Ok(None)` when no session log exists.
///
/// The multi-MB log is NOT re-parsed per call: folded facts are cached per
/// path and extended incrementally — an unchanged (len, mtime) serves from
/// cache with one stat, an append folds only the new complete lines, and a
/// detected rewrite (shrunk file, or an mtime change at unchanged length)
/// re-folds from scratch. The writer is append-only today, so rewrite
/// detection is contract hardening; a rewrite that regrows PAST the cursor
/// within one poll interval is the residual undetected case.
pub fn read_lineage_ledger(
    log_dir: &Path,
    source_session_id: &str,
) -> io::Result<Option<LineageLedger>> {
    let path = log_dir.join("session.jsonl");
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let len = metadata.len();
    let mtime_nanos = mtime_nanos_of(&metadata);

    let cached = lineage_facts_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&path)
        .cloned();

    let entry = match cached {
        // Stat unchanged since the last evaluation: derive straight from the
        // cached facts (also covers an unchanged pending partial tail — no
        // re-read until the file actually moves).
        Some(entry) if entry.stat_len == len && entry.mtime_nanos == mtime_nanos => entry,
        // Grown file: fold only the appended consumable lines. (Strictly
        // greater than the last observed stat length — an mtime change
        // without growth, even while a partial tail left consumed < len, is
        // a rewrite and falls through to the full refold below.)
        Some(entry) if len > entry.stat_len => {
            match read_new_complete_lines_sync(&path, entry.consumed_bytes, len)? {
                Some((text, consumed)) => {
                    let mut facts = entry.facts.clone();
                    for line in text.lines() {
                        fold_session_facts_line(&mut facts, line);
                    }
                    let entry = Arc::new(CachedLineageFacts {
                        consumed_bytes: entry.consumed_bytes + consumed,
                        stat_len: len,
                        mtime_nanos,
                        facts,
                    });
                    store_lineage_facts(&path, entry.clone());
                    entry
                }
                // No consumable new content yet — record the observed stat
                // (so an untouched file serves from cache and a later
                // no-growth mtime change still reads as a rewrite) and
                // evaluate the cached facts.
                None => {
                    let entry = Arc::new(CachedLineageFacts {
                        consumed_bytes: entry.consumed_bytes,
                        stat_len: len,
                        mtime_nanos,
                        facts: entry.facts.clone(),
                    });
                    store_lineage_facts(&path, entry.clone());
                    entry
                }
            }
        }
        // Cold cache, a shrunk file, or a rewrite without growth (mtime
        // moved at the same stat length): fold from scratch.
        _ => {
            let contents = match fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(err) => return Err(err),
            };
            let (complete, consumed) = complete_lines_prefix(&contents);
            let entry = Arc::new(CachedLineageFacts {
                consumed_bytes: consumed,
                stat_len: len,
                mtime_nanos,
                facts: session_facts_from_jsonl(complete),
            });
            store_lineage_facts(&path, entry.clone());
            entry
        }
    };
    Ok(lineage_ledger_from_facts(&entry.facts, source_session_id))
}

/// The byte length of the foldable JSONL prefix of `buf`: every complete
/// (newline-terminated) line, plus an unterminated final line iff it already
/// parses as a complete JSON value. Event lines are JSON objects and no
/// strict prefix of a JSON object parses, so a parsing tail is a whole event
/// whose newline just hasn't landed — or a writer that never terminated its
/// final line, which the historical full re-parse accepted. `None` when
/// nothing is consumable yet. (An async twin lives in the Codex
/// `context_trace.rs` for the trace-index fold.)
fn consumable_jsonl_prefix(buf: &[u8]) -> Option<usize> {
    let complete = buf
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|pos| pos + 1)
        .unwrap_or(0);
    let tail = &buf[complete..];
    let consumed = if !tail.is_empty() && serde_json::from_slice::<serde_json::Value>(tail).is_ok()
    {
        buf.len()
    } else {
        complete
    };
    (consumed > 0).then_some(consumed)
}

/// The foldable prefix of `contents` and its byte length
/// ([`consumable_jsonl_prefix`]).
fn complete_lines_prefix(contents: &str) -> (&str, u64) {
    match consumable_jsonl_prefix(contents.as_bytes()) {
        Some(consumed) => (&contents[..consumed], consumed as u64),
        None => ("", 0),
    }
}

/// Read the consumable lines appended past `offset`
/// ([`consumable_jsonl_prefix`]); `None` when nothing consumable arrived.
/// Never consumes a partially flushed trailing line. The read is clamped to
/// the caller's stat snapshot (`stat_len`) so the recorded mtime stays
/// paired with the consumed length.
fn read_new_complete_lines_sync(
    path: &Path,
    offset: u64,
    stat_len: u64,
) -> io::Result<Option<(String, u64)>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))?;
    }
    let mut buf = Vec::new();
    file.take(stat_len.saturating_sub(offset))
        .read_to_end(&mut buf)?;
    let Some(consumed) = consumable_jsonl_prefix(&buf) else {
        return Ok(None);
    };
    buf.truncate(consumed);
    // The writer emits UTF-8 JSONL; a non-UTF-8 tail is treated as
    // not-yet-complete rather than corrupting the fold.
    match String::from_utf8(buf) {
        Ok(text) => Ok(Some((text, consumed as u64))),
        Err(_) => Ok(None),
    }
}

/// One-shot fold + derive over raw `session.jsonl` contents — the uncached
/// composition of [`session_facts_from_jsonl`] and
/// [`lineage_ledger_from_facts`]. Test surface: production reads go through
/// [`read_lineage_ledger`]'s cache.
#[cfg(test)]
pub fn lineage_ledger_from_jsonl(contents: &str, source_session_id: &str) -> Option<LineageLedger> {
    lineage_ledger_from_facts(&session_facts_from_jsonl(contents), source_session_id)
}

fn session_facts_from_jsonl(contents: &str) -> SessionFacts {
    let mut facts = SessionFacts::default();
    for line in contents.lines() {
        fold_session_facts_line(&mut facts, line);
    }
    facts
}

/// Fold one `session.jsonl` line into the facts — a pure left-fold, so the
/// incremental cache in [`read_lineage_ledger`] can continue exactly where
/// the previous fold stopped.
fn fold_session_facts_line(facts: &mut SessionFacts, line: &str) {
    let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    let event = entry
        .get("event")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let data = entry.get("data").unwrap_or(&serde_json::Value::Null);
    match event {
        "session_identity" => {
            let session_id = json_string(data, "session_id");
            let backend_session_id = json_string(data, "backend_session_id");
            if !session_id.is_empty() && !backend_session_id.is_empty() {
                facts.identities.insert(session_id, backend_session_id);
            }
        }
        "session_started" => {
            let session_id = json_string(data, "session_id");
            let task = json_string(data, "task");
            if !session_id.is_empty() && !task.is_empty() {
                facts.tasks.insert(session_id, task);
            }
        }
        "session_relationship" => {
            let rel = RelationshipKey {
                parent_session_id: json_string(data, "parent_session_id"),
                child_session_id: json_string(data, "child_session_id"),
                relationship: json_string(data, "relationship"),
                ephemeral: data
                    .get("ephemeral")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
            };
            if !rel.parent_session_id.is_empty()
                && !rel.child_session_id.is_empty()
                && !rel.relationship.is_empty()
            {
                facts
                    .relationship_order
                    .insert(rel.clone(), facts.relationship_seq);
                facts.relationship_seq += 1;
                match rel.relationship.as_str() {
                    // Fission detach/import markers prefer updating their
                    // spawn row over becoming rows of their own; whether a
                    // spawn row exists depends on the full relationship set,
                    // so they are resolved at derive time (event order does
                    // not matter).
                    "fission-detached" | "fission-imported" => {
                        let pair = (rel.parent_session_id.clone(), rel.child_session_id.clone());
                        if rel.relationship == "fission-detached" {
                            facts.fission_detached.insert(pair);
                        } else {
                            facts.fission_imported.insert(pair);
                        }
                        facts.pending_fission_marks.push(rel);
                    }
                    _ => {
                        facts.relationships.insert(rel);
                    }
                }
            }
        }
        "done_signal" => {
            let session_id = json_string(data, "session_id");
            if !session_id.is_empty() {
                facts
                    .statuses
                    .insert(session_id.clone(), "completed".into());
                // Ignore the writer's boilerplate default ("Agent signalled
                // done") so it doesn't masquerade as a model-authored summary.
                if let Some(message) = entry
                    .get("message")
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|message| {
                        !message.is_empty() && *message != DONE_SIGNAL_DEFAULT_MESSAGE
                    })
                    .map(trim_summary)
                {
                    facts.summaries.insert(session_id, message);
                }
            }
        }
        "task_complete" => {
            let session_id = json_string(data, "session_id");
            if !session_id.is_empty() {
                facts
                    .statuses
                    .insert(session_id.clone(), "completed".into());
                let summary = data
                    .get("summary")
                    .and_then(|value| value.as_str())
                    .or_else(|| data.get("reason").and_then(|value| value.as_str()))
                    .map(trim_summary);
                if let Some(summary) = summary {
                    facts.summaries.insert(session_id, summary);
                }
            }
        }
        "session_ended" => {
            let session_id = json_string(data, "session_id");
            if !session_id.is_empty() {
                let reason = json_string(data, "reason");
                // A generic teardown must not downgrade a completed task or
                // clobber a model-authored summary with a terse reason.
                if facts.statuses.get(&session_id).map(String::as_str) != Some("completed") {
                    let status = if session_ended_reason_is_failure(&reason) {
                        "failed"
                    } else {
                        "ended"
                    };
                    facts.statuses.insert(session_id.clone(), status.into());
                }
                if !reason.is_empty() && !facts.summaries.contains_key(&session_id) {
                    facts.summaries.insert(session_id, trim_summary(&reason));
                }
            }
        }
        _ => {}
    }
}

/// Derive the lineage ledger for `source_session_id`'s connected component
/// from folded facts — the cheap per-call step over the cached fold.
/// Consumed by [`read_lineage_ledger`] (the dashboard Managed tab / MCP
/// `get_status` read side and the rewind path's lineage snapshot in
/// `main.rs`).
///
/// Branch rows come from `session_relationship` events. Specially handled
/// relationship kinds:
/// - `rewind-restore` — row status `restored`; the latest one becomes the
///   group's canonical head;
/// - `rewind-backout` — row status `inspection`;
/// - `fission-branch` — a fission spawn edge (written by the
///   `register_spawned_branch` / observation wiring); status follows the
///   child's observed lifecycle unless a fission marker overrides it;
/// - `fission-detached` / `fission-imported` — markers that update the spawn
///   row's status (`detached` / `imported`) instead of duplicating the row;
///   they only become rows of their own when the log carries no matching
///   spawn row (see [`fission_status_override`] for the precedence rules).
///
/// Everything else (`subagent`, `managed-edit-branch`, …) renders generically
/// with the child's observed status.
fn lineage_ledger_from_facts(
    facts: &SessionFacts,
    source_session_id: &str,
) -> Option<LineageLedger> {
    // A detach/import marker dedups into its spawn row (`fission-branch`)
    // when one exists — the marker then only drives that row's status — and
    // becomes a standalone row otherwise (e.g. a truncated log that no longer
    // carries the spawn event), so the fact stays visible either way.
    // Resolved per derive (not folded into the base facts) so a spawn event
    // appended after its marker still dedups.
    let mut relationships = facts.relationships.clone();
    for rel in &facts.pending_fission_marks {
        let has_spawn_row = relationships.iter().any(|existing| {
            existing.relationship == "fission-branch"
                && existing.parent_session_id == rel.parent_session_id
                && existing.child_session_id == rel.child_session_id
        });
        if !has_spawn_row {
            relationships.insert(rel.clone());
        }
    }

    if relationships.is_empty() {
        return None;
    }

    let relationships = related_relationships(relationships, source_session_id);
    if relationships.is_empty() {
        return None;
    }

    let mut by_parent: BTreeMap<String, Vec<RelationshipKey>> = BTreeMap::new();
    for rel in relationships {
        by_parent
            .entry(rel.parent_session_id.clone())
            .or_default()
            .push(rel);
    }

    let mut groups = Vec::new();
    for (parent_session_id, relationships) in by_parent {
        let canonical_session_id = relationships
            .iter()
            .filter(|rel| rel.relationship == "rewind-restore")
            .max_by_key(|rel| facts.relationship_order.get(*rel).copied().unwrap_or(0))
            .map(|rel| rel.child_session_id.clone())
            .or_else(|| Some(parent_session_id.clone()));
        let branches = relationships
            .into_iter()
            .map(|rel| {
                let status = if rel.relationship == "rewind-restore" {
                    "restored".to_string()
                } else if rel.relationship == "rewind-backout" {
                    "inspection".to_string()
                } else if let Some(status) =
                    fission_status_override(&facts.fission_detached, &facts.fission_imported, &rel)
                {
                    status
                } else {
                    facts
                        .statuses
                        .get(&rel.child_session_id)
                        .cloned()
                        .unwrap_or_else(|| "running".to_string())
                };
                LineageBranch {
                    backend_session_id: facts.identities.get(&rel.child_session_id).cloned(),
                    task: facts.tasks.get(&rel.child_session_id).cloned(),
                    summary: facts.summaries.get(&rel.child_session_id).cloned(),
                    raw_log: format!("session.jsonl#session_id={}", rel.child_session_id),
                    session_id: rel.child_session_id,
                    relationship: rel.relationship,
                    status,
                    ephemeral: rel.ephemeral,
                }
            })
            .collect();
        groups.push(LineageGroup {
            group_id: format!("session:{parent_session_id}"),
            parent_session_id,
            canonical_session_id,
            branches,
        });
    }
    Some(LineageLedger {
        source_session_id: source_session_id.to_string(),
        groups,
    })
}

/// Status override for fission relationship rows (`fission-branch` /
/// `fission-detached` / `fission-imported`). Precedence mirrors the fission
/// ledger's stickiness rules — `detached` beats `imported` beats the child's
/// observed lifecycle status — so a detach survives both stray completion
/// events from a still-running child and artifact-level imports, and an
/// import is not downgraded by a later generic teardown event. Returns `None`
/// for non-fission rows (markers are scoped to fission edges; a `subagent`
/// edge for the same pair keeps its own lifecycle) and for plain spawn rows
/// without marks, which fall through to the observed status.
fn fission_status_override(
    fission_detached: &BTreeSet<(String, String)>,
    fission_imported: &BTreeSet<(String, String)>,
    rel: &RelationshipKey,
) -> Option<String> {
    if !matches!(
        rel.relationship.as_str(),
        "fission-branch" | "fission-detached" | "fission-imported"
    ) {
        return None;
    }
    let marked = |marks: &BTreeSet<(String, String)>| {
        marks.iter().any(|(parent, child)| {
            parent == &rel.parent_session_id && child == &rel.child_session_id
        })
    };
    if rel.relationship == "fission-detached" || marked(fission_detached) {
        return Some("detached".to_string());
    }
    if rel.relationship == "fission-imported" || marked(fission_imported) {
        return Some("imported".to_string());
    }
    None
}

fn related_relationships(
    relationships: BTreeSet<RelationshipKey>,
    source_session_id: &str,
) -> Vec<RelationshipKey> {
    // An empty source id has no lineage to anchor to; returning *all* relationships
    // would leak unrelated sessions' lineage. Callers always pass a concrete id.
    if source_session_id.trim().is_empty() {
        return Vec::new();
    }

    let mut related: BTreeSet<String> = [source_session_id.to_string()].into_iter().collect();
    loop {
        let before = related.len();
        for rel in &relationships {
            if related.contains(&rel.parent_session_id) || related.contains(&rel.child_session_id) {
                related.insert(rel.parent_session_id.clone());
                related.insert(rel.child_session_id.clone());
            }
        }
        if related.len() == before {
            break;
        }
    }

    relationships
        .into_iter()
        .filter(|rel| {
            related.contains(&rel.parent_session_id) || related.contains(&rel.child_session_id)
        })
        .collect()
}

fn json_string(data: &serde_json::Value, key: &str) -> String {
    data.get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn trim_summary(value: &str) -> String {
    const MAX_CHARS: usize = 240;
    let value = value.trim();
    if value.chars().count() <= MAX_CHARS {
        return value.to_string();
    }
    let mut out: String = value.chars().take(MAX_CHARS).collect();
    out.push_str("...");
    out
}

/// True for `session_ended` reasons that describe a failed branch, matching
/// the fission lifecycle watcher's status split.
fn session_ended_reason_is_failure(reason: &str) -> bool {
    let reason = reason.trim().to_ascii_lowercase();
    reason.starts_with("error") || reason.contains("errored") || reason.contains("failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lineage_ledger_groups_relationships_by_parent() {
        let jsonl = concat!(
            r#"{"event":"session_identity","data":{"session_id":"child","source":"codex","backend_session_id":"thread-child"}}"#,
            "\n",
            r#"{"event":"session_started","data":{"session_id":"child","task":"check the parser"}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"child","relationship":"subagent","ephemeral":false}}"#,
            "\n",
            r#"{"event":"task_complete","data":{"session_id":"child","reason":"done","summary":"parser is fine"}}"#,
            "\n",
        );

        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        assert_eq!(ledger.groups.len(), 1);
        assert_eq!(ledger.groups[0].parent_session_id, "parent");
        assert_eq!(
            ledger.groups[0].canonical_session_id.as_deref(),
            Some("parent")
        );
        let branch = &ledger.groups[0].branches[0];
        assert_eq!(branch.session_id, "child");
        assert_eq!(branch.backend_session_id.as_deref(), Some("thread-child"));
        assert_eq!(branch.relationship, "subagent");
        assert_eq!(branch.status, "completed");
        assert_eq!(branch.task.as_deref(), Some("check the parser"));
        assert_eq!(branch.summary.as_deref(), Some("parser is fine"));
    }

    #[test]
    fn lineage_ledger_session_ended_maps_failure_reasons() {
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-branch","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_ended","data":{"session_id":"branch","reason":"error: model call failed"}}"#,
            "\n",
        );

        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        let branch = &ledger.groups[0].branches[0];
        assert_eq!(branch.status, "failed");
        assert_eq!(branch.summary.as_deref(), Some("error: model call failed"));
    }

    #[test]
    fn lineage_ledger_marks_rewind_restore_as_canonical() {
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"old","child_session_id":"inspect","relationship":"rewind-backout","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"old","child_session_id":"restore","relationship":"rewind-restore","ephemeral":false}}"#,
            "\n",
        );

        let ledger = lineage_ledger_from_jsonl(jsonl, "old").expect("ledger");
        assert_eq!(
            ledger.groups[0].canonical_session_id.as_deref(),
            Some("restore")
        );
        assert_eq!(ledger.groups[0].branches[0].status, "inspection");
        assert_eq!(ledger.groups[0].branches[1].status, "restored");
    }

    #[test]
    fn lineage_ledger_canonical_is_latest_restore_not_lexicographic() {
        // Two restores against the same thread; the second-emitted ("aaa") is the
        // latest and must be canonical even though "zzz" sorts later lexically.
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"old","child_session_id":"zzz","relationship":"rewind-restore","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"old","child_session_id":"aaa","relationship":"rewind-restore","ephemeral":false}}"#,
            "\n",
        );

        let ledger = lineage_ledger_from_jsonl(jsonl, "old").expect("ledger");
        assert_eq!(
            ledger.groups[0].canonical_session_id.as_deref(),
            Some("aaa")
        );
    }

    #[test]
    fn lineage_ledger_empty_source_returns_none() {
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"child","relationship":"subagent","ephemeral":false}}"#,
            "\n",
        );
        assert!(lineage_ledger_from_jsonl(jsonl, "  ").is_none());
    }

    #[test]
    fn lineage_ledger_omits_unrelated_relationship_groups() {
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"child","relationship":"subagent","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"other-parent","child_session_id":"other-child","relationship":"subagent","ephemeral":false}}"#,
            "\n",
        );

        let ledger = lineage_ledger_from_jsonl(jsonl, "child").expect("ledger");
        assert_eq!(ledger.groups.len(), 1);
        assert_eq!(ledger.groups[0].parent_session_id, "parent");
        assert_eq!(ledger.groups[0].branches[0].session_id, "child");
    }

    #[test]
    fn lineage_ledger_parses_fission_branch_relationship() {
        let jsonl = concat!(
            r#"{"event":"session_started","data":{"session_id":"branch","task":"trace the bug"}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-branch","ephemeral":false}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        let branch = &ledger.groups[0].branches[0];
        assert_eq!(branch.relationship, "fission-branch");
        assert_eq!(branch.status, "running");
        assert_eq!(branch.task.as_deref(), Some("trace the bug"));

        // Without markers, a spawn row follows the child's observed lifecycle
        // — and the edge connects the component when sourcing from the child.
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-branch","ephemeral":false}}"#,
            "\n",
            r#"{"event":"task_complete","data":{"session_id":"branch","summary":"traced"}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "branch").expect("ledger");
        let branch = &ledger.groups[0].branches[0];
        assert_eq!(branch.status, "completed");
        assert_eq!(branch.summary.as_deref(), Some("traced"));
    }

    #[test]
    fn lineage_ledger_fission_detached_updates_spawn_row_without_duplicate() {
        // Detach marker plus a stray later completion: one row, sticky
        // detached — mirrors the fission ledger's rule that a detach must
        // survive completion events from a still-running child.
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-branch","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-detached","ephemeral":false}}"#,
            "\n",
            r#"{"event":"task_complete","data":{"session_id":"branch","summary":"finished anyway"}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        assert_eq!(ledger.groups.len(), 1);
        let branches = &ledger.groups[0].branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].relationship, "fission-branch");
        assert_eq!(branches[0].status, "detached");
        // Artifact-level facts (the summary) still render.
        assert_eq!(branches[0].summary.as_deref(), Some("finished anyway"));
    }

    #[test]
    fn lineage_ledger_fission_detached_dedups_regardless_of_event_order() {
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-detached","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-branch","ephemeral":false}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        let branches = &ledger.groups[0].branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].relationship, "fission-branch");
        assert_eq!(branches[0].status, "detached");
    }

    #[test]
    fn lineage_ledger_fission_detached_without_spawn_row_gets_own_row() {
        // A truncated log may carry the detach marker but not the spawn
        // event; the fact must stay visible as a standalone row.
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-detached","ephemeral":false}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        let branches = &ledger.groups[0].branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].relationship, "fission-detached");
        assert_eq!(branches[0].status, "detached");
    }

    #[test]
    fn lineage_ledger_fission_imported_marks_spawn_row_status() {
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-branch","ephemeral":false}}"#,
            "\n",
            r#"{"event":"task_complete","data":{"session_id":"branch","summary":"useful diff"}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-imported","ephemeral":false}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        let branches = &ledger.groups[0].branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].relationship, "fission-branch");
        assert_eq!(branches[0].status, "imported");
        assert_eq!(branches[0].summary.as_deref(), Some("useful diff"));
    }

    #[test]
    fn lineage_ledger_import_does_not_resurrect_detached_fission_branch() {
        // Import is artifact-level: a detached branch whose result was
        // salvaged stays detached, mirroring the fission ledger.
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-branch","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-detached","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-imported","ephemeral":false}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        let branches = &ledger.groups[0].branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].status, "detached");
    }

    #[test]
    fn lineage_ledger_fission_imported_without_spawn_row_gets_own_row() {
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-imported","ephemeral":false}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "branch").expect("ledger");
        let branches = &ledger.groups[0].branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].relationship, "fission-imported");
        assert_eq!(branches[0].status, "imported");
    }

    #[test]
    fn lineage_ledger_fission_marks_do_not_touch_non_fission_rows() {
        // Markers are scoped to fission edges: a subagent edge for the same
        // (parent, child) keeps its own lifecycle status, while the marker —
        // having no spawn row to fold into — renders standalone.
        let jsonl = concat!(
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"subagent","ephemeral":false}}"#,
            "\n",
            r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"branch","relationship":"fission-detached","ephemeral":false}}"#,
            "\n",
            r#"{"event":"task_complete","data":{"session_id":"branch","summary":"done"}}"#,
            "\n",
        );
        let ledger = lineage_ledger_from_jsonl(jsonl, "parent").expect("ledger");
        let branches = &ledger.groups[0].branches;
        assert_eq!(branches.len(), 2);
        let subagent = branches
            .iter()
            .find(|branch| branch.relationship == "subagent")
            .unwrap();
        let detached = branches
            .iter()
            .find(|branch| branch.relationship == "fission-detached")
            .unwrap();
        assert_eq!(subagent.status, "completed");
        assert_eq!(detached.status, "detached");
    }

    #[test]
    fn read_lineage_ledger_folds_appends_incrementally_and_refolds_on_truncation() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let rel_line = r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"child","relationship":"subagent","ephemeral":false}}"#;
        let done_line = r#"{"event":"task_complete","data":{"session_id":"child","reason":"done","summary":"parser is fine"}}"#;
        std::fs::write(&path, format!("{rel_line}\n")).unwrap();

        // Missing log dir: clean None.
        assert!(read_lineage_ledger(&dir.path().join("missing"), "parent")
            .unwrap()
            .is_none());

        let ledger = read_lineage_ledger(dir.path(), "parent")
            .unwrap()
            .expect("ledger");
        assert_eq!(ledger.groups[0].branches[0].status, "running");

        // Append (as the writer does) — the cached fold extends and the new
        // fact shows up.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{done_line}").unwrap();
        drop(file);
        let ledger = read_lineage_ledger(dir.path(), "parent")
            .unwrap()
            .expect("ledger");
        assert_eq!(ledger.groups[0].branches[0].status, "completed");
        assert_eq!(
            ledger.groups[0].branches[0].summary.as_deref(),
            Some("parser is fine")
        );

        // A partial trailing line (writer mid-flush) is invisible until its
        // newline lands, then folds exactly once.
        let second_rel = r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"child-2","relationship":"subagent","ephemeral":false}}"#;
        let (head, tail) = second_rel.split_at(second_rel.len() / 2);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        write!(file, "{head}").unwrap();
        drop(file);
        let ledger = read_lineage_ledger(dir.path(), "parent")
            .unwrap()
            .expect("ledger");
        assert_eq!(ledger.groups[0].branches.len(), 1);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{tail}").unwrap();
        drop(file);
        let ledger = read_lineage_ledger(dir.path(), "parent")
            .unwrap()
            .expect("ledger");
        assert_eq!(ledger.groups[0].branches.len(), 2);

        // Truncation (never produced by the append-only writer, but the
        // cache must not serve stale facts): full refold.
        std::fs::write(&path, format!("{rel_line}\n")).unwrap();
        let ledger = read_lineage_ledger(dir.path(), "parent")
            .unwrap()
            .expect("ledger");
        assert_eq!(ledger.groups[0].branches.len(), 1);
        assert_eq!(ledger.groups[0].branches[0].status, "running");
    }

    fn set_mtime(path: &Path, secs_since_epoch: u64) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs_since_epoch))
            .unwrap();
    }

    #[test]
    fn read_lineage_ledger_refolds_same_length_rewrite_while_partial_tail_pending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let rel_a = r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"child-a","relationship":"subagent","ephemeral":false}}"#;
        let rel_b = r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"child-b","relationship":"subagent","ephemeral":false}}"#;
        assert_eq!(rel_a.len(), rel_b.len(), "test needs equal-length lines");
        let done = r#"{"event":"task_complete","data":{"session_id":"child-a","summary":"ok"}}"#;
        let (partial, _rest) = done.split_at(done.len() / 2);

        // Cached state with a pending partial tail: consumed < stat len.
        std::fs::write(&path, format!("{rel_a}\n{partial}")).unwrap();
        set_mtime(&path, 1_000);
        let ledger = read_lineage_ledger(dir.path(), "parent")
            .unwrap()
            .expect("ledger");
        assert_eq!(ledger.groups[0].branches[0].session_id, "child-a");

        // Same TOTAL length, different content and mtime: a pure
        // consumed<=len append model would tail-read from the stale cursor;
        // the recorded stat length classifies it as a rewrite instead.
        std::fs::write(&path, format!("{rel_b}\n{partial}")).unwrap();
        set_mtime(&path, 2_000);
        let ledger = read_lineage_ledger(dir.path(), "parent")
            .unwrap()
            .expect("ledger");
        assert_eq!(ledger.groups[0].branches[0].session_id, "child-b");
    }

    #[test]
    fn read_lineage_ledger_refolds_on_same_length_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let rel_line = r#"{"event":"session_relationship","data":{"parent_session_id":"parent","child_session_id":"child","relationship":"subagent","ephemeral":false}}"#;
        let done_a = r#"{"event":"task_complete","data":{"session_id":"child","reason":"done","summary":"parser is fine"}}"#;
        let done_b = r#"{"event":"task_complete","data":{"session_id":"child","reason":"done","summary":"parser is FINE"}}"#;
        assert_eq!(done_a.len(), done_b.len(), "test needs equal-length lines");

        std::fs::write(&path, format!("{rel_line}\n{done_a}\n")).unwrap();
        set_mtime(&path, 1_000);
        let ledger = read_lineage_ledger(dir.path(), "parent")
            .unwrap()
            .expect("ledger");
        assert_eq!(
            ledger.groups[0].branches[0].summary.as_deref(),
            Some("parser is fine")
        );

        // Same total length, different content, different mtime: the pure
        // (consumed <= len) model would read zero new bytes and serve the
        // stale facts forever.
        std::fs::write(&path, format!("{rel_line}\n{done_b}\n")).unwrap();
        set_mtime(&path, 2_000);
        let ledger = read_lineage_ledger(dir.path(), "parent")
            .unwrap()
            .expect("ledger");
        assert_eq!(
            ledger.groups[0].branches[0].summary.as_deref(),
            Some("parser is FINE")
        );
    }
}
