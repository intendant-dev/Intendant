//! Fork-a-session-from-an-anchor: the unified fork-point catalog.
//!
//! A **fork point** is a place in a session's history where a new session
//! can be forked off ("resume from any branch, from any anchor"). Fork
//! points are always derived from the backend's own canonical transcript —
//! the Codex rollout JSONL, the Claude Code project transcript, the native
//! `conversation.jsonl` — never from Intendant's diagnostic mirror, matching
//! every existing rewind/fork path. Deriving is read-only; the fork engines
//! (later phases) only ever *copy* parent artifacts.
//!
//! Backend vocabulary mapping:
//! - **codex**: managed-context rewind anchors (item ids from the rollout
//!   scan, `managed_context_ops::anchors`) plus whole-turn boundaries. On a
//!   vanilla binary a fork lands on turn boundaries (`thread/fork{path}` +
//!   `thread/rollback{numTurns}`), so item anchors are annotated with the
//!   turn boundary they round down to; the managed fork binary keeps exact
//!   item-anchor cuts.
//! - **intendant** (native): round boundaries of the persisted
//!   `conversation.jsonl`, keyed by the `seq` of the last kept message.
//! - **claude-code**: message-boundary anchors on the transcript's uuid
//!   chain, including inactive sibling branch tips (a follow-up phase; the
//!   catalog reports `supported: false` until the tree parser lands).

mod claude_tree;
mod fork_points;
mod native;
pub(crate) use claude_tree::*;
pub(crate) use fork_points::*;
pub(crate) use native::*;

use serde::Serialize;

/// One place a fork can cut at, backend-tagged via `kind`/`granularity`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ForkPoint {
    /// Stable list identity for the UI (`turn:3`, `item:<id>:after`,
    /// `seq:42`, `head`).
    pub(crate) id: String,
    /// `turn-boundary` | `item-anchor` | `round` | `head` (`message` /
    /// `branch-tip` once the Claude Code arm lands).
    pub(crate) kind: &'static str,
    /// The precision a fork at this point actually gets: `turn`, `item`,
    /// or `round`.
    pub(crate) granularity: &'static str,
    /// 1-based user-turn / round ordinal this point relates to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    /// Native only: `seq` of the last message the fork keeps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Codex only: the rollout item id of the anchor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// Codex only: cut position relative to the item (`before`/`after`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) position: Option<&'static str>,
    /// Claude Code only: the transcript message uuid the fork keeps
    /// history through (the chain-slice anchor).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_uuid: Option<String>,
    /// Claude Code only: the anchor sits on history replaced by the
    /// newest compact boundary. Informational — the chain-slice fork
    /// omits the boundary and keeps full pre-compaction history.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) pre_compaction: bool,
    pub(crate) preview: String,
    pub(crate) eligible: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) eligibility_reasons: Vec<String>,
    /// Where the cut actually lands when the requested point is more
    /// precise than the backend supports (vanilla codex item anchors).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) effective_cut: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ForkPointCatalog {
    pub(crate) session_id: String,
    /// `intendant` | `codex` | `claude-code` | `gemini`.
    pub(crate) source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backend_session_id: Option<String>,
    pub(crate) supported: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) unsupported_reason: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) notes: Vec<String>,
    /// Total fork points before paging.
    pub(crate) total: usize,
    pub(crate) offset: usize,
    pub(crate) limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_offset: Option<usize>,
    pub(crate) fork_points: Vec<ForkPoint>,
}

impl ForkPointCatalog {
    pub(crate) fn unsupported(
        session_id: &str,
        source: &str,
        backend_session_id: Option<&str>,
        reason: &str,
    ) -> Self {
        Self {
            session_id: session_id.to_string(),
            source: source.to_string(),
            backend_session_id: backend_session_id.map(str::to_string),
            supported: false,
            unsupported_reason: Some(reason.to_string()),
            notes: Vec::new(),
            total: 0,
            offset: 0,
            limit: 0,
            next_offset: None,
            fork_points: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ForkPointQuery {
    /// Also list codex item anchors the managed rewind eligibility filter
    /// would hide (headroom-ineligible ones). Fork itself needs no
    /// headroom; this mirrors the managed catalog's diagnostic escape
    /// hatch so the default list stays focused.
    pub(crate) include_non_recovery: bool,
    pub(crate) offset: usize,
    pub(crate) limit: usize,
}

pub(crate) const FORK_POINT_DEFAULT_LIMIT: usize = 200;
pub(crate) const FORK_POINT_MAX_LIMIT: usize = 1000;

impl Default for ForkPointQuery {
    fn default() -> Self {
        Self {
            include_non_recovery: false,
            offset: 0,
            limit: FORK_POINT_DEFAULT_LIMIT,
        }
    }
}

/// The wire form of a chosen fork point, carried by
/// `ControlMsg::ForkSessionAtAnchor`. Field usage mirrors `ForkPoint`:
/// codex anchors use `item_id`/`position` or `turn`, native uses `seq`,
/// claude-code uses `message_uuid`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ForkAnchorSpec {
    /// Fork-point kind from the catalog (`turn-boundary`, `item-anchor`,
    /// `round`, `head`, `message`, `branch-tip`).
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_uuid: Option<String>,
}

impl ForkAnchorSpec {
    /// One-line summary for result events and logs.
    pub(crate) fn summary(&self) -> String {
        let mut parts = vec![self.kind.clone()];
        if let Some(turn) = self.turn {
            parts.push(format!("turn {turn}"));
        }
        if let Some(seq) = self.seq {
            parts.push(format!("seq {seq}"));
        }
        if let Some(item_id) = &self.item_id {
            let position = self.position.as_deref().unwrap_or("after");
            parts.push(format!("{position} {item_id}"));
        }
        if let Some(uuid) = &self.message_uuid {
            parts.push(format!("through {uuid}"));
        }
        parts.join(", ")
    }
}

/// First ~140 chars of `text`, whitespace collapsed to single spaces —
/// the one-line preview shown next to a fork point.
pub(crate) fn fork_point_preview(text: &str) -> String {
    let mut out = String::with_capacity(140);
    let mut last_space = true;
    for ch in text.chars() {
        let ch = if ch.is_whitespace() { ' ' } else { ch };
        if ch == ' ' && last_space {
            continue;
        }
        last_space = ch == ' ';
        out.push(ch);
        if out.len() >= 140 {
            out.push('…');
            break;
        }
    }
    out.trim_end().to_string()
}

/// Apply `offset`/`limit` paging to an already-ordered fork-point list,
/// filling the catalog's paging fields.
pub(crate) fn page_fork_points(
    catalog: &mut ForkPointCatalog,
    points: Vec<ForkPoint>,
    query: &ForkPointQuery,
) {
    let total = points.len();
    let offset = query.offset.min(total);
    let limit = query.limit.clamp(1, FORK_POINT_MAX_LIMIT);
    let end = offset.saturating_add(limit).min(total);
    catalog.total = total;
    catalog.offset = offset;
    catalog.limit = limit;
    catalog.next_offset = (end < total).then_some(end);
    catalog.fork_points = points[offset..end].to_vec();
}
