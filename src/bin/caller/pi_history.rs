//! Read-only Pi session-history adapter.
//!
//! Pi 0.81.x persists one append-only JSONL tree per native session:
//!
//! ```text
//! $PI_CODING_AGENT_DIR/
//!   sessions/--<encoded-cwd>--/<timestamp>_<session-id>.jsonl
//! ```
//!
//! The first row is a v3 `session` header. Every later row has an `id` and
//! `parentId`; the last row is the active leaf, so abandoned sibling branches
//! remain in the file without belonging to the active conversation. This
//! module owns those format facts for the catalog, replay, resume and search
//! lanes. It never launches Pi and only consults process environment at the
//! real-home production edge; tests with injected homes stay hermetic.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub(crate) const PI_SOURCE: &str = "pi";
pub(crate) const PI_SOURCE_LABEL: &str = "Pi";
pub(crate) const PI_SESSION_SCAN_LIMIT: usize = 2_000;
const PI_SESSION_CANDIDATE_LIMIT: usize = PI_SESSION_SCAN_LIMIT * 4;
const PI_SCAN_ENTRY_LIMIT: usize = 40_000;
const PI_SCAN_MAX_DEPTH: usize = 3;
const PI_HEADER_READ_LIMIT: u64 = 1024 * 1024;
const PI_SESSION_READ_LIMIT: u64 = 128 * 1024 * 1024;
const PI_SESSION_RECORD_LIMIT: usize = 200_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PiUsage {
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_write: u64,
    pub(crate) total: u64,
}

impl PiUsage {
    fn from_value(value: &Value) -> Self {
        let input = value.get("input").and_then(Value::as_u64).unwrap_or(0);
        let output = value.get("output").and_then(Value::as_u64).unwrap_or(0);
        let cache_read = value.get("cacheRead").and_then(Value::as_u64).unwrap_or(0);
        let cache_write = value.get("cacheWrite").and_then(Value::as_u64).unwrap_or(0);
        let accounted = input
            .saturating_add(output)
            .saturating_add(cache_read)
            .saturating_add(cache_write);
        let total = value
            .get("totalTokens")
            .and_then(Value::as_u64)
            .unwrap_or(accounted)
            .max(accounted);
        Self {
            input,
            output,
            cache_read,
            cache_write,
            total,
        }
    }

    fn add(&mut self, other: Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.total = self.total.saturating_add(other.total);
    }

    pub(crate) fn is_empty(self) -> bool {
        self == Self::default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PiUsageRecord {
    pub(crate) timestamp: Option<String>,
    pub(crate) usage: PiUsage,
}

#[derive(Clone, Debug)]
pub(crate) struct PiSessionLocation {
    pub(crate) agent_dir: PathBuf,
    pub(crate) path: PathBuf,
    pub(crate) session_id: String,
    pub(crate) cwd: Option<String>,
    pub(crate) created_at: Option<String>,
    pub(crate) parent_session: Option<PathBuf>,
    pub(crate) updated_millis: i64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PiSessionHistory {
    pub(crate) location: Option<PiSessionLocation>,
    pub(crate) name: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) thinking: Option<String>,
    pub(crate) first_prompt: Option<String>,
    pub(crate) turns: u64,
    pub(crate) updated_at: Option<String>,
    pub(crate) usage: PiUsage,
    pub(crate) usage_records: Vec<PiUsageRecord>,
    pub(crate) entries: Vec<Value>,
    pub(crate) consumed_bytes: u64,
}

impl PiSessionHistory {
    /// Rows on Pi's active parent chain, in conversation order. A legacy file
    /// with no tree ids preserves physical order. Once ids exist, mirror Pi's
    /// own path builder: a missing parent ends the usable chain instead of
    /// turning every physical sibling into active conversation history.
    pub(crate) fn active_entries(&self) -> Vec<&Value> {
        let mut by_id = HashMap::<&str, usize>::new();
        let mut leaf = None;
        for (index, entry) in self.entries.iter().enumerate() {
            if let Some(id) = non_empty_str(entry.get("id")) {
                by_id.insert(id, index);
                leaf = Some(index);
            }
        }
        let Some(mut index) = leaf else {
            return self.entries.iter().collect();
        };
        let mut path = Vec::new();
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(index) {
                break;
            }
            let entry = &self.entries[index];
            path.push(entry);
            let Some(parent_id) = non_empty_str(entry.get("parentId")) else {
                break;
            };
            let Some(parent) = by_id.get(parent_id).copied() else {
                break;
            };
            index = parent;
        }
        path.reverse();
        path
    }
}

pub(crate) fn pi_agent_dir_in(home: &Path) -> PathBuf {
    if home != crate::platform::home_dir() {
        return home.join(".pi").join("agent");
    }
    std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(".pi").join("agent"))
}

/// Pi roots visible to the current daemon. An active OAuth lease uses a
/// private synthesized agent directory, while standalone/subscription login
/// history remains under the configured/default Pi directory.
pub(crate) fn pi_agent_roots_in(home: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if home == crate::platform::home_dir() {
        if let Some(root) = crate::credential_leases::materialized_pi_agent_dir() {
            push_unique_path(&mut roots, root);
        }
    }
    push_unique_path(&mut roots, pi_agent_dir_in(home));
    roots
}

pub(crate) fn list_pi_sessions_from_home(home: &Path, limit: usize) -> Vec<PiSessionLocation> {
    let limit = limit.min(PI_SESSION_SCAN_LIMIT);
    if limit == 0 {
        return Vec::new();
    }
    let mut newest = HashMap::<String, PiSessionLocation>::new();
    for agent_dir in pi_agent_roots_in(home) {
        for location in list_pi_sessions_in(&agent_dir, limit.saturating_mul(4)) {
            let replace = newest
                .get(&location.session_id)
                .map(|current| location.updated_millis > current.updated_millis)
                .unwrap_or(true);
            if replace {
                newest.insert(location.session_id.clone(), location);
            }
        }
    }
    let mut locations = newest.into_values().collect::<Vec<_>>();
    locations.sort_by_key(|location| std::cmp::Reverse(location.updated_millis));
    locations.truncate(limit);
    locations
}

pub(crate) fn find_pi_session_from_home(
    home: &Path,
    session_id: &str,
) -> Option<PiSessionLocation> {
    let session_id = session_id.trim();
    if !is_pi_session_id(session_id) {
        return None;
    }
    let mut newest = None::<PiSessionLocation>;
    for agent_dir in pi_agent_roots_in(home) {
        if let Some(location) = find_pi_session_in(&agent_dir, session_id) {
            let replace = newest
                .as_ref()
                .map(|current| location.updated_millis > current.updated_millis)
                .unwrap_or(true);
            if replace {
                newest = Some(location);
            }
        }
    }
    newest
}

pub(crate) fn list_pi_sessions_in(agent_dir: &Path, limit: usize) -> Vec<PiSessionLocation> {
    let mut locations = recent_session_files(agent_dir, limit)
        .into_iter()
        .filter_map(|path| pi_session_location(agent_dir, &path))
        .collect::<Vec<_>>();
    locations.sort_by_key(|location| std::cmp::Reverse(location.updated_millis));
    locations.truncate(limit.min(PI_SESSION_SCAN_LIMIT));
    locations
}

pub(crate) fn find_pi_session_in(agent_dir: &Path, session_id: &str) -> Option<PiSessionLocation> {
    let session_id = session_id.trim();
    if !is_pi_session_id(session_id) {
        return None;
    }
    recent_session_files(agent_dir, PI_SESSION_CANDIDATE_LIMIT)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(session_id))
        })
        .filter_map(|path| pi_session_location(agent_dir, &path))
        .filter(|location| location.session_id == session_id)
        .max_by_key(|location| location.updated_millis)
}

pub(crate) fn parse_pi_session(location: PiSessionLocation) -> PiSessionHistory {
    let Some(file) = std::fs::File::open(&location.path).ok() else {
        return PiSessionHistory {
            location: Some(location),
            ..PiSessionHistory::default()
        };
    };
    let mut history = PiSessionHistory {
        updated_at: location.created_at.clone(),
        location: Some(location),
        ..PiSessionHistory::default()
    };
    let mut reader = std::io::BufReader::new(file.take(PI_SESSION_READ_LIMIT));
    let mut line = String::new();
    let mut records = 0usize;
    while records < PI_SESSION_RECORD_LIMIT {
        line.clear();
        let Ok(consumed) = reader.read_line(&mut line) else {
            break;
        };
        if consumed == 0 {
            break;
        }
        if !line.ends_with('\n') {
            break;
        }
        history.consumed_bytes = history.consumed_bytes.saturating_add(consumed as u64);
        records += 1;
        let Ok(entry) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) == Some("session") {
            continue;
        }
        let timestamp = pi_entry_timestamp(&entry);
        if timestamp_is_newer(timestamp.as_deref(), history.updated_at.as_deref()) {
            history.updated_at = timestamp.clone();
        }
        match entry.get("type").and_then(Value::as_str).unwrap_or("") {
            "session_info" => {
                history.name = non_empty_str(entry.get("name")).map(str::to_string);
            }
            "message" => {
                fold_message(&entry, timestamp, &mut history);
            }
            "compaction" | "branch_summary" => {
                if let Some(value) = entry.get("usage") {
                    record_usage(value, timestamp, &mut history);
                }
            }
            _ => {}
        }
        history.entries.push(entry);
    }
    derive_active_context_settings(&mut history);
    history
}

pub(crate) fn parse_pi_session_file(path: &Path) -> Option<PiSessionHistory> {
    let sessions_dir = path
        .ancestors()
        .find(|ancestor| ancestor.file_name().and_then(|name| name.to_str()) == Some("sessions"))?;
    let agent_dir = sessions_dir.parent()?;
    let location = pi_session_location(agent_dir, path)?;
    Some(parse_pi_session(location))
}

pub(crate) fn is_pi_session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub(crate) fn pi_message_text(message: &Value) -> String {
    crate::external_agent::pi::message_text(message)
}

pub(crate) fn pi_entry_timestamp(entry: &Value) -> Option<String> {
    let message_millis = entry
        .get("message")
        .and_then(|message| message.get("timestamp"))
        .and_then(Value::as_i64);
    message_millis
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .map(|timestamp| timestamp.to_rfc3339())
        .or_else(|| non_empty_str(entry.get("timestamp")).map(str::to_string))
}

fn fold_message(entry: &Value, timestamp: Option<String>, history: &mut PiSessionHistory) {
    let Some(message) = entry.get("message") else {
        return;
    };
    match message.get("role").and_then(Value::as_str).unwrap_or("") {
        "user" => {
            history.turns = history.turns.saturating_add(1);
            let text = pi_message_text(message);
            if history.first_prompt.is_none() && !text.trim().is_empty() {
                history.first_prompt = Some(text);
            }
        }
        "assistant" => {
            if let Some(value) = message.get("usage") {
                record_usage(value, timestamp, history);
            }
        }
        "toolResult" => {
            if let Some(value) = message.get("usage") {
                record_usage(value, timestamp, history);
            }
        }
        _ => {}
    }
}

/// Pi rebuilds model and thinking state from the selected leaf's parent
/// chain, not from the last matching row in physical file order. Usage stays
/// physical because abandoned branches were still billed; runtime context
/// settings must not be contaminated by a sibling the user branched away
/// from. Session names are deliberately left physical/latest above, matching
/// Pi's own `getSessionName()` treatment of that session-wide metadata.
fn derive_active_context_settings(history: &mut PiSessionHistory) {
    let mut provider = None;
    let mut model = None;
    let mut thinking = None;
    for entry in history.active_entries() {
        match entry.get("type").and_then(Value::as_str) {
            Some("thinking_level_change") => {
                thinking = non_empty_str(entry.get("thinkingLevel")).map(str::to_string);
            }
            Some("model_change") => {
                provider = non_empty_str(entry.get("provider")).map(str::to_string);
                model = non_empty_str(entry.get("modelId")).map(str::to_string);
            }
            Some("message") => {
                let Some(message) = entry.get("message") else {
                    continue;
                };
                if message.get("role").and_then(Value::as_str) == Some("assistant") {
                    provider = non_empty_str(message.get("provider")).map(str::to_string);
                    model = non_empty_str(message.get("responseModel"))
                        .or_else(|| non_empty_str(message.get("model")))
                        .map(str::to_string);
                }
            }
            _ => {}
        }
    }
    history.provider = provider;
    history.model = model;
    history.thinking = thinking;
}

fn record_usage(value: &Value, timestamp: Option<String>, history: &mut PiSessionHistory) {
    let usage = PiUsage::from_value(value);
    if usage.is_empty() {
        return;
    }
    history.usage.add(usage);
    history
        .usage_records
        .push(PiUsageRecord { timestamp, usage });
}

fn pi_session_location(agent_dir: &Path, path: &Path) -> Option<PiSessionLocation> {
    let header = read_pi_session_header(path)?;
    let session_id = non_empty_str(header.get("id"))?.to_string();
    if !is_pi_session_id(&session_id) {
        return None;
    }
    Some(PiSessionLocation {
        agent_dir: agent_dir.to_path_buf(),
        path: path.to_path_buf(),
        session_id,
        cwd: non_empty_str(header.get("cwd")).map(str::to_string),
        created_at: non_empty_str(header.get("timestamp")).map(str::to_string),
        parent_session: non_empty_str(header.get("parentSession")).map(PathBuf::from),
        updated_millis: file_mtime_millis(path),
    })
}

pub(crate) fn read_pi_session_header(path: &Path) -> Option<Value> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file.take(PI_HEADER_READ_LIMIT));
    let mut line = String::new();
    while reader.read_line(&mut line).ok()? > 0 {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            let value = serde_json::from_str::<Value>(trimmed).ok()?;
            return (value.get("type").and_then(Value::as_str) == Some("session")).then_some(value);
        }
        line.clear();
    }
    None
}

fn recent_session_files(agent_dir: &Path, limit: usize) -> Vec<PathBuf> {
    let mut candidates = Vec::<(i64, PathBuf)>::new();
    let mut visited = 0usize;
    collect_session_files(
        &agent_dir.join("sessions"),
        0,
        &mut visited,
        &mut candidates,
    );
    candidates.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    candidates.truncate(limit.min(PI_SESSION_CANDIDATE_LIMIT));
    candidates.into_iter().map(|(_, path)| path).collect()
}

fn collect_session_files(
    dir: &Path,
    depth: usize,
    visited: &mut usize,
    out: &mut Vec<(i64, PathBuf)>,
) {
    if depth > PI_SCAN_MAX_DEPTH || *visited >= PI_SCAN_ENTRY_LIMIT {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *visited >= PI_SCAN_ENTRY_LIMIT {
            break;
        }
        *visited += 1;
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            collect_session_files(&path, depth + 1, visited, out);
        } else if file_type.is_file()
            && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
        {
            out.push((file_mtime_millis(&path), path));
        }
    }
}

fn file_mtime_millis(path: &Path) -> i64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn timestamp_is_newer(candidate: Option<&str>, current: Option<&str>) -> bool {
    match (candidate, current) {
        (Some(candidate), Some(current)) => timestamp_millis(candidate) > timestamp_millis(current),
        (Some(_), None) => true,
        _ => false,
    }
}

fn timestamp_millis(value: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.timestamp_millis())
        .unwrap_or(0)
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    if paths
        .iter()
        .any(|existing| std::fs::canonicalize(existing).unwrap_or_else(|_| existing.clone()) == key)
    {
        return;
    }
    paths.push(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(root: &Path, id: &str) -> PathBuf {
        let dir = root.join(".pi/agent/sessions/--tmp-project--");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("2026-07-21T00-00-00-000Z_{id}.jsonl"));
        let lines = [
            serde_json::json!({
                "type":"session", "version":3, "id":id,
                "timestamp":"2026-07-21T00:00:00Z", "cwd":"/tmp/project"
            }),
            serde_json::json!({
                "type":"message", "id":"a", "parentId":null,
                "timestamp":"2026-07-21T00:00:01Z",
                "message":{"role":"user","content":"build it","timestamp":1784592001000i64}
            }),
            serde_json::json!({
                "type":"message", "id":"b", "parentId":"a",
                "timestamp":"2026-07-21T00:00:02Z",
                "message":{"role":"assistant","provider":"openai-codex","model":"gpt-5.6-codex",
                    "content":[{"type":"text","text":"done"}],
                    "usage":{"input":10,"output":3,"cacheRead":4,"cacheWrite":2,"totalTokens":19},
                    "stopReason":"stop","timestamp":1784592002000i64}
            }),
            serde_json::json!({
                "type":"message", "id":"abandoned", "parentId":"a",
                "timestamp":"2026-07-21T00:00:03Z",
                "message":{"role":"assistant","provider":"anthropic","model":"stale-model",
                    "content":[{"type":"text","text":"abandoned"}],
                    "usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2},
                    "stopReason":"stop","timestamp":1784592003000i64}
            }),
            serde_json::json!({
                "type":"thinking_level_change", "id":"active-thinking", "parentId":"b",
                "timestamp":"2026-07-21T00:00:03.100Z", "thinkingLevel":"low"
            }),
            serde_json::json!({
                "type":"model_change", "id":"stale-model", "parentId":"abandoned",
                "timestamp":"2026-07-21T00:00:03.200Z", "provider":"anthropic", "modelId":"stale-model-2"
            }),
            serde_json::json!({
                "type":"thinking_level_change", "id":"stale-thinking", "parentId":"stale-model",
                "timestamp":"2026-07-21T00:00:03.300Z", "thinkingLevel":"max"
            }),
            serde_json::json!({
                "type":"session_info", "id":"name", "parentId":"active-thinking",
                "timestamp":"2026-07-21T00:00:04Z", "name":"Pi fixture"
            }),
        ];
        let body = lines
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, format!("{body}\n")).unwrap();
        path
    }

    #[test]
    fn lists_parses_and_follows_active_tree() {
        let home = tempfile::tempdir().unwrap();
        let path = write_session(home.path(), "01JTEST_PI");
        let locations = list_pi_sessions_from_home(home.path(), 10);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].path, path);
        assert_eq!(locations[0].cwd.as_deref(), Some("/tmp/project"));

        let history = parse_pi_session(locations[0].clone());
        assert_eq!(history.name.as_deref(), Some("Pi fixture"));
        assert_eq!(history.first_prompt.as_deref(), Some("build it"));
        assert_eq!(history.turns, 1);
        assert_eq!(history.model.as_deref(), Some("gpt-5.6-codex"));
        assert_eq!(history.provider.as_deref(), Some("openai-codex"));
        assert_eq!(history.thinking.as_deref(), Some("low"));
        assert_eq!(
            history.usage.total, 21,
            "physical-file usage includes branches"
        );
        let active_ids = history
            .active_entries()
            .iter()
            .filter_map(|entry| entry.get("id").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(active_ids, vec!["a", "b", "active-thinking", "name"]);
    }

    #[test]
    fn injected_home_ignores_process_pi_directory() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            pi_agent_roots_in(home.path()),
            vec![home.path().join(".pi/agent")]
        );
    }

    #[test]
    fn torn_final_row_is_not_treated_as_the_active_leaf() {
        let home = tempfile::tempdir().unwrap();
        let path = write_session(home.path(), "01J_TORN_PI");
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(
            br#"{"type":"message","id":"torn","parentId":"name","message":{"role":"user","content":"unfinished"}}"#,
        )
        .unwrap();

        let location = find_pi_session_in(&home.path().join(".pi/agent"), "01J_TORN_PI")
            .expect("complete header still locates the session");
        let history = parse_pi_session(location);
        let active_ids = history
            .active_entries()
            .iter()
            .filter_map(|entry| entry.get("id").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(active_ids, vec!["a", "b", "active-thinking", "name"]);
        assert!(history
            .entries
            .iter()
            .all(|entry| entry.get("id").and_then(Value::as_str) != Some("torn")));
        assert!(history.consumed_bytes < std::fs::metadata(path).unwrap().len());
    }

    #[test]
    fn validates_upstream_session_id_grammar() {
        for valid in ["a", "01J.test-id_2"] {
            assert!(is_pi_session_id(valid), "{valid}");
        }
        for invalid in ["", "-bad", "bad-", "../bad", "has space"] {
            assert!(!is_pi_session_id(invalid), "{invalid}");
        }
    }

    #[test]
    fn missing_parent_does_not_activate_physical_siblings() {
        let history = PiSessionHistory {
            entries: vec![
                serde_json::json!({"type":"message","id":"root","parentId":null}),
                serde_json::json!({"type":"message","id":"sibling","parentId":"root"}),
                serde_json::json!({"type":"message","id":"orphan","parentId":"missing"}),
            ],
            ..PiSessionHistory::default()
        };
        let active_ids = history
            .active_entries()
            .iter()
            .filter_map(|entry| entry.get("id").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(active_ids, vec!["orphan"]);
    }
}
