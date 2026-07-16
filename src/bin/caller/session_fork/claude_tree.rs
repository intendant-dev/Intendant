//! Claude Code transcript tree: the uuid/parentUuid message graph inside a
//! `~/.claude/projects/**/<uuid>.jsonl` session file.
//!
//! Semantics pinned by the session-fork probes (CC 2.1.211,
//! `tests/skills/session-fork-probes/`): resume walks the chain back from
//! the TAIL message (legacy `last-prompt` pins are dead), sibling branches
//! coexist in one file as divergent chains, and a `compact_boundary`
//! system line with `parentUuid: null` severs the walk (its
//! `logicalParentUuid` records the pre-compaction chain it replaced).

use std::collections::HashMap;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct ClaudeTreeNode {
    pub(crate) uuid: String,
    pub(crate) parent_uuid: Option<String>,
    /// `user` / `assistant` / `system:<subtype>` / the raw `type` field.
    pub(crate) kind: String,
    pub(crate) preview: String,
    pub(crate) ts: Option<String>,
    pub(crate) line_no: usize,
    pub(crate) is_sidechain: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ClaudeCompactBoundary {
    pub(crate) uuid: String,
    pub(crate) logical_parent_uuid: Option<String>,
    pub(crate) line_no: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ClaudeTranscriptTree {
    pub(crate) nodes: Vec<ClaudeTreeNode>,
    pub(crate) by_uuid: HashMap<String, usize>,
    pub(crate) children: HashMap<String, Vec<usize>>,
    /// The uuid resume would walk back from: the last non-sidechain
    /// message-bearing line in file order.
    pub(crate) active_leaf: Option<String>,
    pub(crate) newest_compact_boundary: Option<ClaudeCompactBoundary>,
}

impl ClaudeTranscriptTree {
    pub(crate) fn node(&self, uuid: &str) -> Option<&ClaudeTreeNode> {
        self.by_uuid.get(uuid).map(|&index| &self.nodes[index])
    }

    /// `uuid` plus its ancestors, leaf-to-root order. Cycle-safe.
    pub(crate) fn ancestor_chain(&self, uuid: &str) -> Vec<&ClaudeTreeNode> {
        let mut chain = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut cursor = Some(uuid.to_string());
        while let Some(current) = cursor {
            if !seen.insert(current.clone()) {
                break;
            }
            let Some(node) = self.node(&current) else {
                break;
            };
            chain.push(node);
            cursor = node.parent_uuid.clone();
        }
        chain
    }

    /// Non-sidechain user/assistant leaves, file order. Sidechain
    /// children don't count against leaf-ness — a message whose only
    /// descendants are sidechain work is still a resumable chain tip.
    pub(crate) fn message_leaves(&self) -> Vec<&ClaudeTreeNode> {
        self.nodes
            .iter()
            .filter(|node| !node.is_sidechain)
            .filter(|node| node.kind == "user" || node.kind == "assistant")
            .filter(|node| {
                self.children
                    .get(&node.uuid)
                    .is_none_or(|kids| kids.iter().all(|&index| self.nodes[index].is_sidechain))
            })
            .collect()
    }

    /// Whether a fork anchored at `uuid` keeps only pre-compaction
    /// history. The transcript is append-only and the boundary replaced
    /// everything before it, so "written before the newest boundary
    /// line" is the honest chronological test — it also covers abandoned
    /// sibling branches rooted in compacted history, which are not on
    /// the boundary's own ancestor chain. Such forks are still fully
    /// supported — the chain-slice never includes the boundary — this is
    /// informational for the UI.
    pub(crate) fn anchor_is_pre_compaction(&self, uuid: &str) -> bool {
        let Some(boundary) = &self.newest_compact_boundary else {
            return false;
        };
        self.node(uuid)
            .is_some_and(|node| node.line_no < boundary.line_no)
    }
}

fn node_preview(value: &serde_json::Value) -> String {
    let message = value.get("message");
    let text = message
        .and_then(|message| message.get("content"))
        .map(|content| match content {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Array(blocks) => blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        })
        .unwrap_or_default();
    let fallback = value
        .get("content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    super::fork_point_preview(if text.is_empty() { fallback } else { &text })
}

pub(crate) fn parse_claude_transcript_tree(path: &Path) -> io::Result<ClaudeTranscriptTree> {
    let file = std::fs::File::open(path)?;
    let reader = io::BufReader::new(file);
    let mut tree = ClaudeTranscriptTree::default();

    for (line_index, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Torn tails (a live writer mid-append) and foreign lines parse
        // as errors; both are skipped rather than failing the scan.
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let Some(uuid) = value
            .get("uuid")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            continue; // meta lines (ai-title, agent-name, …) carry no uuid
        };
        let line_type = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let subtype = value.get("subtype").and_then(serde_json::Value::as_str);
        let kind = match (line_type, subtype) {
            ("system", Some(subtype)) => format!("system:{subtype}"),
            (other, _) => other.to_string(),
        };
        let is_sidechain = value
            .get("isSidechain")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let parent_uuid = value
            .get("parentUuid")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let node = ClaudeTreeNode {
            uuid: uuid.clone(),
            parent_uuid: parent_uuid.clone(),
            kind: kind.clone(),
            preview: node_preview(&value),
            ts: value
                .get("timestamp")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            line_no: line_index + 1,
            is_sidechain,
        };
        let index = tree.nodes.len();
        tree.nodes.push(node);
        tree.by_uuid.insert(uuid.clone(), index);
        if let Some(parent) = parent_uuid {
            tree.children.entry(parent).or_default().push(index);
        }
        if kind == "system:compact_boundary" {
            tree.newest_compact_boundary = Some(ClaudeCompactBoundary {
                uuid: uuid.clone(),
                logical_parent_uuid: value
                    .get("logicalParentUuid")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                line_no: line_index + 1,
            });
        }
        if !is_sidechain && (kind == "user" || kind == "assistant") {
            tree.active_leaf = Some(uuid);
        }
    }
    Ok(tree)
}

/// Shared (possibly cached) tree scan, copying the managed-context
/// anchor-scan cache discipline (`managed_context_ops::anchors`): keyed by
/// (path, len, change stamp), served/cached only when the stamp is
/// quiescent past the racy-write window, fingerprint re-verified after the
/// read. CC transcripts reach tens of MB and the catalog + surgery paths
/// re-read them back to back.
pub(crate) fn shared_claude_tree_scan(
    path: &Path,
) -> io::Result<std::sync::Arc<ClaudeTranscriptTree>> {
    const SLOTS: usize = 4;
    const RACY_WINDOW_NANOS: i128 = 2_000_000_000;

    struct CachedTree {
        path: PathBuf,
        len: u64,
        stamp: crate::platform::FileChangeStamp,
        tree: std::sync::Arc<ClaudeTranscriptTree>,
    }
    static CACHE: std::sync::Mutex<Vec<CachedTree>> = std::sync::Mutex::new(Vec::new());

    fn fingerprint(path: &Path) -> io::Result<(u64, Option<crate::platform::FileChangeStamp>)> {
        let meta = std::fs::metadata(path)?;
        let stamp = crate::platform::file_change_stamp(path, &meta);
        Ok((meta.len(), stamp))
    }
    fn quiescent(stamp: &crate::platform::FileChangeStamp) -> bool {
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0);
        stamp
            .change_signal_unix_nanos()
            .saturating_add(RACY_WINDOW_NANOS)
            <= now_nanos
    }

    let (len, stamp) = fingerprint(path)?;
    let stamp = stamp.filter(quiescent);
    if let Some(stamp) = stamp {
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(index) = cache
            .iter()
            .position(|entry| entry.len == len && entry.stamp == stamp && entry.path == path)
        {
            let hit = cache.remove(index);
            let tree = std::sync::Arc::clone(&hit.tree);
            cache.insert(0, hit);
            return Ok(tree);
        }
    }
    let tree = std::sync::Arc::new(parse_claude_transcript_tree(path)?);
    if let Some(stamp) = stamp {
        let unchanged = fingerprint(path).is_ok_and(|after| after == (len, Some(stamp)));
        if unchanged {
            let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            cache.retain(|entry| entry.path != path);
            cache.insert(
                0,
                CachedTree {
                    path: path.to_path_buf(),
                    len,
                    stamp,
                    tree: std::sync::Arc::clone(&tree),
                },
            );
            cache.truncate(SLOTS);
        }
    }
    Ok(tree)
}

#[cfg(test)]
pub(crate) mod test_fixtures {
    /// One CC transcript line. `parent: None` renders `parentUuid: null`.
    pub(crate) fn message_line(
        uuid: &str,
        parent: Option<&str>,
        kind: &str,
        text: &str,
        sidechain: bool,
    ) -> String {
        serde_json::json!({
            "uuid": uuid,
            "parentUuid": parent,
            "type": kind,
            "isSidechain": sidechain,
            "sessionId": "fixture-session",
            "timestamp": "2026-07-16T00:00:00.000Z",
            "message": {"role": kind, "content": text},
        })
        .to_string()
    }

    pub(crate) fn boundary_line(uuid: &str, logical_parent: &str) -> String {
        serde_json::json!({
            "uuid": uuid,
            "parentUuid": null,
            "logicalParentUuid": logical_parent,
            "type": "system",
            "subtype": "compact_boundary",
            "isSidechain": false,
            "sessionId": "fixture-session",
            "timestamp": "2026-07-16T00:00:00.000Z",
            "content": "Conversation compacted",
            "compactMetadata": {"trigger": "auto", "preTokens": 1000, "postTokens": 100},
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixtures::*;
    use super::*;

    fn write_transcript(lines: &[String]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir
            .path()
            .join("11111111-0000-0000-0000-000000000000.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").expect("write");
        (dir, path)
    }

    #[test]
    fn linear_chain_parses_with_tail_active_leaf() {
        let (_dir, path) = write_transcript(&[
            "{\"type\":\"ai-title\",\"aiTitle\":\"t\",\"sessionId\":\"s\"}".to_string(),
            message_line("u1", None, "user", "round one", false),
            message_line("a1", Some("u1"), "assistant", "answer one", false),
            message_line("u2", Some("a1"), "user", "round two", false),
        ]);
        let tree = parse_claude_transcript_tree(&path).expect("tree");
        assert_eq!(tree.nodes.len(), 3);
        assert_eq!(tree.active_leaf.as_deref(), Some("u2"));
        let chain: Vec<&str> = tree
            .ancestor_chain("u2")
            .iter()
            .map(|node| node.uuid.as_str())
            .collect();
        assert_eq!(chain, vec!["u2", "a1", "u1"]);
    }

    #[test]
    fn sibling_branches_yield_inactive_leaves() {
        let (_dir, path) = write_transcript(&[
            message_line("u1", None, "user", "root", false),
            message_line("a1", Some("u1"), "assistant", "branch A", false),
            message_line("a2", Some("u1"), "assistant", "branch B", false),
        ]);
        let tree = parse_claude_transcript_tree(&path).expect("tree");
        assert_eq!(tree.active_leaf.as_deref(), Some("a2"));
        let leaves: Vec<&str> = tree
            .message_leaves()
            .iter()
            .map(|node| node.uuid.as_str())
            .collect();
        assert_eq!(leaves, vec!["a1", "a2"]);
    }

    #[test]
    fn compact_boundary_severs_walk_and_marks_pre_compaction() {
        let (_dir, path) = write_transcript(&[
            message_line("u1", None, "user", "old round", false),
            message_line("a1", Some("u1"), "assistant", "old answer", false),
            boundary_line("b1", "a1"),
            message_line("u2", Some("b1"), "user", "post-compact round", false),
        ]);
        let tree = parse_claude_transcript_tree(&path).expect("tree");
        assert_eq!(tree.active_leaf.as_deref(), Some("u2"));
        // The walk from the active leaf stops at the boundary's null parent.
        let chain: Vec<&str> = tree
            .ancestor_chain("u2")
            .iter()
            .map(|node| node.uuid.as_str())
            .collect();
        assert_eq!(chain, vec!["u2", "b1"]);
        assert!(tree.anchor_is_pre_compaction("a1"));
        assert!(tree.anchor_is_pre_compaction("u1"));
        assert!(!tree.anchor_is_pre_compaction("u2"));
    }

    #[test]
    fn sidechains_are_excluded_from_leaves_and_active_leaf() {
        let (_dir, path) = write_transcript(&[
            message_line("u1", None, "user", "main", false),
            message_line("s1", Some("u1"), "assistant", "sidechain work", true),
        ]);
        let tree = parse_claude_transcript_tree(&path).expect("tree");
        assert_eq!(tree.active_leaf.as_deref(), Some("u1"));
        let leaves: Vec<&str> = tree
            .message_leaves()
            .iter()
            .map(|node| node.uuid.as_str())
            .collect();
        assert_eq!(leaves, vec!["u1"]);
    }

    #[test]
    fn torn_tail_is_tolerated() {
        let mut lines = vec![
            message_line("u1", None, "user", "round", false),
            message_line("a1", Some("u1"), "assistant", "answer", false),
        ];
        lines.push("{\"uuid\":\"torn".to_string());
        let (_dir, path) = write_transcript(&lines);
        let tree = parse_claude_transcript_tree(&path).expect("tree");
        assert_eq!(tree.nodes.len(), 2);
        assert_eq!(tree.active_leaf.as_deref(), Some("a1"));
    }

    #[test]
    fn shared_scan_returns_parsed_tree() {
        let (_dir, path) = write_transcript(&[message_line("u1", None, "user", "round", false)]);
        let tree = shared_claude_tree_scan(&path).expect("scan");
        assert_eq!(tree.nodes.len(), 1);
    }
}
