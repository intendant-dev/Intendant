use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::Path;

/// Boilerplate the session log writes for a `done_signal` with no caller message
/// (see `SessionLog::done_signal_for_session`). Filtered out so it isn't treated
/// as a model-authored branch summary.
const DONE_SIGNAL_DEFAULT_MESSAGE: &str = "Agent signalled done";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineageLedger {
    pub source_session_id: String,
    pub groups: Vec<LineageGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineageGroup {
    pub group_id: String,
    pub parent_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_session_id: Option<String>,
    pub branches: Vec<LineageBranch>,
}

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

#[derive(Default)]
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
}

pub fn read_lineage_ledger(
    log_dir: &Path,
    source_session_id: &str,
) -> io::Result<Option<LineageLedger>> {
    let path = log_dir.join("session.jsonl");
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    Ok(lineage_ledger_from_jsonl(&contents, source_session_id))
}

pub fn lineage_ledger_from_jsonl(contents: &str, source_session_id: &str) -> Option<LineageLedger> {
    let mut facts = SessionFacts::default();
    let mut relationship_seq = 0usize;
    for line in contents.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
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
                    facts.relationship_order.insert(rel.clone(), relationship_seq);
                    relationship_seq += 1;
                    facts.relationships.insert(rel);
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
                    // A generic teardown must not downgrade a completed task or
                    // clobber a model-authored summary with a terse reason.
                    if facts.statuses.get(&session_id).map(String::as_str) != Some("completed") {
                        facts.statuses.insert(session_id.clone(), "ended".into());
                    }
                    let reason = json_string(data, "reason");
                    if !reason.is_empty() && !facts.summaries.contains_key(&session_id) {
                        facts.summaries.insert(session_id, trim_summary(&reason));
                    }
                }
            }
            _ => {}
        }
    }

    if facts.relationships.is_empty() {
        return None;
    }

    let relationships = related_relationships(facts.relationships, source_session_id);
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
}
