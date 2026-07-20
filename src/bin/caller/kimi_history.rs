//! Read-only Kimi Code session-history adapter.
//!
//! Kimi persists one session directory per native `session_<uuid>`:
//!
//! ```text
//! $KIMI_CODE_HOME/
//!   session_index.jsonl
//!   sessions/<workdir-key>/session_<uuid>/
//!     state.json
//!     agents/main/wire.jsonl
//!     agents/<agent-id>/wire.jsonl
//! ```
//!
//! `state.json` owns session/agent relationships while each agent wire owns
//! that agent's messages, reasoning, tools, usage, steering and undo records.
//! This module is deliberately transport-free and environment-free except for
//! [`kimi_home_in`], the production edge. Catalog and message-search readers
//! share the parser so they cannot drift on identity or supersession rules.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, Read};
use std::path::{Component, Path, PathBuf};

pub(crate) const KIMI_SOURCE: &str = "kimi";
pub(crate) const KIMI_SOURCE_LABEL: &str = "Kimi";
pub(crate) const KIMI_HOME_DIR: &str = ".kimi-code";
pub(crate) const KIMI_MAIN_AGENT: &str = "main";
const KIMI_SESSION_PREFIX: &str = "session_";
const KIMI_INDEX_READ_LIMIT: u64 = 8 * 1024 * 1024;
const KIMI_INDEX_RECORD_READ_LIMIT: usize = 256 * 1024;
const KIMI_INDEX_RECORD_LIMIT: usize = 100_000;
const KIMI_STATE_READ_LIMIT: u64 = 2 * 1024 * 1024;
const KIMI_WIRE_READ_LIMIT: u64 = 64 * 1024 * 1024;
const KIMI_WIRE_RECORD_READ_LIMIT: usize = 2 * 1024 * 1024;
const KIMI_WIRE_RECORD_LIMIT: usize = 100_000;
const KIMI_SESSION_WIRE_READ_LIMIT: u64 = 128 * 1024 * 1024;
const KIMI_SESSION_WIRE_RECORD_LIMIT: usize = 200_000;
const KIMI_AGENT_LIMIT: usize = 256;
pub(crate) const KIMI_SESSION_SCAN_LIMIT: usize = 2_000;
const KIMI_SESSION_CANDIDATE_LIMIT: usize = KIMI_SESSION_SCAN_LIMIT * 4;
const KIMI_FIND_SESSION_LIMIT: usize = KIMI_SESSION_CANDIDATE_LIMIT;
const KIMI_SCAN_ENTRY_LIMIT: usize = 20_000;
const KIMI_SCAN_MAX_DEPTH: usize = 4;

/// Resolve Kimi's home for a caller-supplied user home.
///
/// Explicit/injected homes stay hermetic: only the actual process home may
/// consult `KIMI_CODE_HOME`, matching the Codex catalog resolver's rule.
pub(crate) fn kimi_home_in(home: &Path) -> PathBuf {
    if home != crate::platform::home_dir() {
        return home.join(KIMI_HOME_DIR);
    }
    std::env::var_os("KIMI_CODE_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| home.join(KIMI_HOME_DIR))
}

/// Every Kimi history root visible from an Intendant/user home.
///
/// Persisted per-session bridge homes come first so a live copy-backed
/// bridge (notably Windows without symlink privileges) wins over an older
/// primary-home copy. The ordinary environment/default root remains the
/// final fallback for standalone Kimi sessions and pre-bridge history.
pub(crate) fn kimi_home_roots_in(home: &Path) -> Vec<PathBuf> {
    let mut roots = crate::session_config::persisted_kimi_homes_in(home);
    let default = kimi_home_in(home);
    if default.is_dir() && !roots.contains(&default) {
        roots.push(default);
    }
    roots
}

/// Locate one native Kimi session across persisted bridge homes and the
/// ordinary Kimi home.
pub(crate) fn find_kimi_session_from_home(
    home: &Path,
    requested_id: &str,
) -> Option<KimiSessionLocation> {
    let mut newest = None::<KimiSessionLocation>;
    for root in kimi_home_roots_in(home) {
        let Some(candidate) = find_kimi_session_in(&root, requested_id) else {
            continue;
        };
        let replace = newest
            .as_ref()
            .map(|current| candidate.activity_mtime() > current.activity_mtime())
            .unwrap_or(true);
        if replace {
            newest = Some(candidate);
        }
    }
    newest
}

/// Enumerate Kimi sessions across all discoverable homes, de-duplicating
/// bridge/primary mirrors by native session id.
pub(crate) fn list_kimi_sessions_from_home(home: &Path, limit: usize) -> Vec<KimiSessionLocation> {
    let result_limit = limit.min(KIMI_SESSION_SCAN_LIMIT);
    if result_limit == 0 {
        return Vec::new();
    }
    let mut sessions = HashMap::<String, KimiSessionLocation>::new();
    for root in kimi_home_roots_in(home) {
        for location in list_kimi_sessions_in(&root, result_limit) {
            let replace = sessions
                .get(&location.session_id)
                .map(|current| location.activity_mtime() > current.activity_mtime())
                .unwrap_or(true);
            if replace {
                sessions.insert(location.session_id.clone(), location);
            }
        }
    }
    let mut sessions = sessions.into_values().collect::<Vec<_>>();
    sessions.sort_by_key(|session| {
        std::cmp::Reverse(
            session
                .updated_at
                .as_deref()
                .and_then(parse_timestamp_ms)
                .unwrap_or_else(|| session.activity_mtime()),
        )
    });
    sessions.truncate(result_limit);
    sessions
}

pub(crate) fn is_kimi_session_id(id: &str) -> bool {
    id.strip_prefix(KIMI_SESSION_PREFIX).is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    })
}

pub(crate) fn kimi_child_session_id(session_id: &str, agent_id: &str) -> Option<String> {
    if !is_kimi_session_id(session_id) || !is_kimi_agent_id(agent_id) || agent_id == KIMI_MAIN_AGENT
    {
        return None;
    }
    Some(format!("{session_id}:{agent_id}"))
}

pub(crate) fn split_kimi_session_id(id: &str) -> Option<(&str, Option<&str>)> {
    if is_kimi_session_id(id) {
        return Some((id, None));
    }
    let (session_id, agent_id) = id.rsplit_once(':')?;
    if !is_kimi_session_id(session_id) || !is_kimi_agent_id(agent_id) || agent_id == KIMI_MAIN_AGENT
    {
        return None;
    }
    Some((session_id, Some(agent_id)))
}

fn is_kimi_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[derive(Clone, Debug)]
pub(crate) struct KimiAgentLocation {
    pub(crate) id: String,
    pub(crate) parent_id: Option<String>,
    pub(crate) agent_type: Option<String>,
    pub(crate) wire_path: PathBuf,
}

impl KimiAgentLocation {
    pub(crate) fn subagent(&self) -> bool {
        self.id != KIMI_MAIN_AGENT
    }
}

#[derive(Clone, Debug)]
pub(crate) struct KimiSessionLocation {
    pub(crate) session_id: String,
    pub(crate) session_dir: PathBuf,
    pub(crate) state_path: PathBuf,
    pub(crate) created_at: Option<String>,
    pub(crate) updated_at: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) last_prompt: Option<String>,
    pub(crate) work_dir: Option<String>,
    pub(crate) agents: Vec<KimiAgentLocation>,
}

impl KimiSessionLocation {
    pub(crate) fn selected_agent(&self, requested_id: &str) -> Option<&KimiAgentLocation> {
        let (session_id, requested_agent) = split_kimi_session_id(requested_id)?;
        if session_id != self.session_id {
            return None;
        }
        let agent_id = requested_agent.unwrap_or(KIMI_MAIN_AGENT);
        self.agents.iter().find(|agent| agent.id == agent_id)
    }

    pub(crate) fn all_dependency_paths(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.state_path.as_path())
            .chain(self.agents.iter().map(|agent| agent.wire_path.as_path()))
    }

    pub(crate) fn activity_mtime(&self) -> i64 {
        std::iter::once(self.session_dir.as_path())
            .chain(self.all_dependency_paths())
            .map(path_activity_mtime)
            .max()
            .unwrap_or(0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct KimiUsage {
    pub(crate) input_other: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_creation: u64,
}

impl KimiUsage {
    pub(crate) fn total(self) -> u64 {
        self.input_other
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_creation)
    }

    fn add(&mut self, other: Self) {
        self.input_other = self.input_other.saturating_add(other.input_other);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_creation = self.cache_creation.saturating_add(other.cache_creation);
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct KimiAgentHistory {
    pub(crate) agent_id: String,
    pub(crate) parent_agent_id: Option<String>,
    pub(crate) subagent: bool,
    /// Bytes consumed through the last complete JSONL line. Message search
    /// captures its cursor at this exact boundary so an append racing the
    /// parse is observed on the next sweep instead of being skipped.
    pub(crate) consumed_bytes: u64,
    pub(crate) created_at: Option<String>,
    pub(crate) updated_at: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) turns: u64,
    pub(crate) active_real_user_turns: u32,
    /// Stable digest of the active real-user revision chain plus the current
    /// compaction/clear generation. Assistant/tool appends deliberately do
    /// not perturb it, while undo+replacement and a new context boundary do.
    pub(crate) head_fingerprint: String,
    /// Active real-user prompts after the latest compaction/clear floor.
    /// Kimi's native undo refuses to cross that floor.
    pub(crate) undoable_real_user_turns: u32,
    pub(crate) has_undo_boundary: bool,
    pub(crate) active_turn_revisions: Vec<u32>,
    pub(crate) undo_floor: u32,
    pub(crate) context_generation: u64,
    pub(crate) first_prompt: Option<String>,
    pub(crate) usage: KimiUsage,
    pub(crate) daily_usage: BTreeMap<String, KimiUsage>,
    pub(crate) entries: Vec<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TurnRef {
    index: u32,
    revision: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct KimiTurnHorizon {
    pub(crate) active_turns: u32,
    pub(crate) undoable_turns: u32,
    pub(crate) has_boundary: bool,
    pub(crate) head_fingerprint: String,
    /// Compact proof material for deriving the exact native horizon after a
    /// tail rollback. Absent on horizons serialized by older Intendant
    /// builds; those remain comparable at their current head but fail closed
    /// when asked to predict a post-undo target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rollback_proof: Option<KimiRollbackProof>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct KimiRollbackProof {
    active_revisions: Vec<u32>,
    undo_floor: u32,
    context_generation: u64,
}

impl PartialEq for KimiTurnHorizon {
    fn eq(&self, other: &Self) -> bool {
        self.active_turns == other.active_turns
            && self.undoable_turns == other.undoable_turns
            && self.has_boundary == other.has_boundary
            && self.head_fingerprint == other.head_fingerprint
    }
}

impl Eq for KimiTurnHorizon {}

impl KimiTurnHorizon {
    /// Derive the exact horizon Kimi must expose after a native tail undo.
    ///
    /// `None` means the count crosses the compaction/clear floor or the
    /// horizon came from an older serialized frame without proof material.
    pub(crate) fn after_rollback(&self, count: u32) -> Option<Self> {
        if count == 0 {
            return Some(self.clone());
        }
        if count > self.undoable_turns {
            return None;
        }
        let proof = self.rollback_proof.as_ref()?;
        if proof.active_revisions.len() != self.active_turns as usize
            || proof.undo_floor as usize > proof.active_revisions.len()
        {
            return None;
        }
        let keep = proof.active_revisions.len().checked_sub(count as usize)?;
        if keep < proof.undo_floor as usize {
            return None;
        }
        let active_turns = proof
            .active_revisions
            .iter()
            .take(keep)
            .enumerate()
            .map(|(index, revision)| TurnRef {
                index: (index + 1).min(u32::MAX as usize) as u32,
                revision: *revision,
            })
            .collect::<Vec<_>>();
        let mut next_proof = proof.clone();
        next_proof.active_revisions.truncate(keep);
        Some(Self {
            active_turns: keep.min(u32::MAX as usize) as u32,
            undoable_turns: keep
                .saturating_sub(proof.undo_floor as usize)
                .min(u32::MAX as usize) as u32,
            has_boundary: self.has_boundary,
            head_fingerprint: active_turn_fingerprint(
                &active_turns,
                proof.undo_floor as usize,
                proof.context_generation,
            ),
            rollback_proof: Some(next_proof),
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct KimiSessionHistory {
    pub(crate) location: KimiSessionLocation,
    pub(crate) agents: Vec<KimiAgentHistory>,
}

impl KimiSessionHistory {
    pub(crate) fn selected_agent(&self, requested_id: &str) -> Option<&KimiAgentHistory> {
        let (session_id, requested_agent) = split_kimi_session_id(requested_id)?;
        if session_id != self.location.session_id {
            return None;
        }
        let agent_id = requested_agent.unwrap_or(KIMI_MAIN_AGENT);
        self.agents.iter().find(|agent| agent.agent_id == agent_id)
    }
}

/// Enumerate canonical Kimi session directories from both the append-only
/// index and a bounded on-disk walk. The walk is authoritative fallback:
/// leased/staged homes may carry `sessions/` without `session_index.jsonl`.
pub(crate) fn list_kimi_sessions_in(kimi_home: &Path, limit: usize) -> Vec<KimiSessionLocation> {
    let result_limit = limit.min(KIMI_SESSION_SCAN_LIMIT);
    if result_limit == 0 {
        return Vec::new();
    }
    let candidate_limit = result_limit
        .saturating_mul(4)
        .min(KIMI_SESSION_CANDIDATE_LIMIT)
        .max(result_limit);
    let sessions_root = kimi_home.join("sessions");
    let mut candidates: HashMap<String, PathBuf> = HashMap::new();

    for (session_id, indexed_path) in read_session_index(kimi_home, candidate_limit) {
        if let Some(path) = safe_indexed_session_dir(&sessions_root, &indexed_path, &session_id) {
            candidates.insert(session_id, path);
        }
    }

    let mut found = Vec::new();
    let mut entry_budget = KIMI_SCAN_ENTRY_LIMIT;
    collect_session_dirs(
        &sessions_root,
        KIMI_SCAN_MAX_DEPTH,
        &mut found,
        candidate_limit,
        &mut entry_budget,
    );
    for path in found {
        let Some(session_id) = session_id_from_dir(&path) else {
            continue;
        };
        let replace = candidates
            .get(&session_id)
            .map(|current| {
                session_candidate_activity_mtime(&path) > session_candidate_activity_mtime(current)
            })
            .unwrap_or(true);
        if replace {
            candidates.insert(session_id, path);
        }
    }

    // Index and walk can each contribute their full bounded candidate set.
    // Rank before opening state files so a large stale index cannot turn a
    // small catalog request into thousands of multi-megabyte parses.
    let mut candidates = candidates.into_iter().collect::<Vec<_>>();
    candidates.sort_by_key(|(_, path)| std::cmp::Reverse(session_candidate_activity_mtime(path)));
    candidates.truncate(candidate_limit);
    let mut sessions = candidates
        .into_iter()
        .filter_map(|(session_id, dir)| parse_session_location(&session_id, &dir))
        .collect::<Vec<_>>();
    sessions.sort_by_key(|session| {
        std::cmp::Reverse(
            session
                .updated_at
                .as_deref()
                .and_then(parse_timestamp_ms)
                .unwrap_or_else(|| session.activity_mtime()),
        )
    });
    sessions.truncate(result_limit);
    sessions
}

pub(crate) fn find_kimi_session_in(
    kimi_home: &Path,
    requested_id: &str,
) -> Option<KimiSessionLocation> {
    let (session_id, requested_agent) = split_kimi_session_id(requested_id)?;
    let sessions_root = kimi_home.join("sessions");

    if let Some((_, indexed_path)) = read_session_index(kimi_home, KIMI_SESSION_CANDIDATE_LIMIT)
        .into_iter()
        .find(|(id, _)| id == session_id)
    {
        if let Some(path) = safe_indexed_session_dir(&sessions_root, &indexed_path, session_id) {
            if let Some(location) = parse_session_location(session_id, &path) {
                if requested_agent.is_none()
                    || location
                        .agents
                        .iter()
                        .any(|agent| Some(agent.id.as_str()) == requested_agent)
                {
                    return Some(location);
                }
            }
        }
    }

    let mut dirs = Vec::new();
    let mut entry_budget = KIMI_SCAN_ENTRY_LIMIT;
    collect_session_dirs(
        &sessions_root,
        KIMI_SCAN_MAX_DEPTH,
        &mut dirs,
        KIMI_FIND_SESSION_LIMIT,
        &mut entry_budget,
    );
    dirs.into_iter()
        .filter(|dir| session_id_from_dir(dir).as_deref() == Some(session_id))
        .filter_map(|dir| {
            parse_session_location(session_id, &dir).filter(|location| {
                requested_agent.is_none()
                    || location
                        .agents
                        .iter()
                        .any(|agent| Some(agent.id.as_str()) == requested_agent)
            })
        })
        .max_by_key(KimiSessionLocation::activity_mtime)
}

pub(crate) fn parse_kimi_session(location: KimiSessionLocation) -> KimiSessionHistory {
    let mut remaining_bytes = KIMI_SESSION_WIRE_READ_LIMIT;
    let mut remaining_records = KIMI_SESSION_WIRE_RECORD_LIMIT;
    let mut agents = Vec::with_capacity(location.agents.len());
    for agent in &location.agents {
        let wire_bytes = std::fs::metadata(&agent.wire_path)
            .ok()
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len());
        let Some(wire_bytes) =
            wire_bytes.filter(|bytes| *bytes <= KIMI_WIRE_READ_LIMIT && *bytes <= remaining_bytes)
        else {
            agents.push(empty_agent_history(agent));
            continue;
        };
        remaining_bytes = remaining_bytes.saturating_sub(wire_bytes);
        agents.push(parse_agent_wire(agent, &mut remaining_records));
    }
    synthesize_subagent_terminals(&mut agents);
    KimiSessionHistory { location, agents }
}

/// Seed the live supervision lane from Kimi's own persisted rewind frame.
///
/// This intentionally replays the source events instead of taking the
/// largest rendered turn ordinal: after `context.undo`, Kimi reuses the
/// dropped turn number with a higher revision, exactly like
/// `UserTurnRevisionState`.
pub(crate) fn kimi_user_turn_state_from_history(
    home: &Path,
    requested_id: &str,
) -> Option<crate::codex_history::UserTurnRevisionState> {
    let location = find_kimi_session_from_home(home, requested_id)?;
    let selected = location.selected_agent(requested_id)?;
    user_turn_state_from_wire(&selected.wire_path)
}

/// Source-of-truth validation frame for direct/caller-supplied Kimi
/// anchor forks. Catalog omission is a convenience, not an authority:
/// planners must reject a rollback deeper than `undoable_turns`.
pub(crate) fn kimi_turn_horizon_from_history(
    home: &Path,
    requested_id: &str,
) -> Option<KimiTurnHorizon> {
    let location = find_kimi_session_from_home(home, requested_id)?;
    let parsed = parse_kimi_session(location);
    let selected = parsed.selected_agent(requested_id)?;
    Some(turn_horizon(selected))
}

/// Resolve a horizon from an already-resolved Kimi data home. The supervised
/// adapter uses this against its bridge both immediately before and after a
/// native fork, closing the planner-to-fork race without consulting process
/// globals or the user's unrelated home.
pub(crate) fn kimi_turn_horizon_in(
    kimi_home: &Path,
    requested_id: &str,
) -> Option<KimiTurnHorizon> {
    let location = find_kimi_session_in(kimi_home, requested_id)?;
    let parsed = parse_kimi_session(location);
    let selected = parsed.selected_agent(requested_id)?;
    Some(turn_horizon(selected))
}

fn turn_horizon(selected: &KimiAgentHistory) -> KimiTurnHorizon {
    KimiTurnHorizon {
        active_turns: selected.active_real_user_turns,
        undoable_turns: selected.undoable_real_user_turns,
        has_boundary: selected.has_undo_boundary,
        head_fingerprint: selected.head_fingerprint.clone(),
        rollback_proof: Some(KimiRollbackProof {
            active_revisions: selected.active_turn_revisions.clone(),
            undo_floor: selected.undo_floor,
            context_generation: selected.context_generation,
        }),
    }
}

fn user_turn_state_from_wire(
    wire_path: &Path,
) -> Option<crate::codex_history::UserTurnRevisionState> {
    let (lines, _) = read_complete_jsonl_lines(wire_path).ok()?;
    let has_prompt_lane = lines.iter().any(|line| {
        line.contains("\"turn.prompt\"")
            && serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|value| string_at(&value, "type"))
                .as_deref()
                == Some("turn.prompt")
    });
    let mut state = crate::codex_history::UserTurnRevisionState::default();
    let mut undo_floor = 0u32;
    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "turn.prompt" if is_real_user_origin(value.get("origin")) => {
                state.record_next_turn();
            }
            "context.append_message"
                if !has_prompt_lane
                    && string_pointer(&value, "/message/role").as_deref() == Some("user")
                    && is_real_user_origin(value.pointer("/message/origin")) =>
            {
                state.record_next_turn();
            }
            "context.apply_compaction" | "context.clear" => {
                undo_floor = state.active_count();
            }
            "context.undo" => {
                let count = value
                    .get("count")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .min(u64::from(u32::MAX)) as u32;
                state.rewind_last_turns(count.min(state.active_count().saturating_sub(undo_floor)));
            }
            _ => {}
        }
    }
    Some(state)
}

fn synthesize_subagent_terminals(agents: &mut [KimiAgentHistory]) {
    let Some(main) = agents
        .iter()
        .find(|agent| agent.agent_id == KIMI_MAIN_AGENT)
    else {
        return;
    };
    let mut terminal_by_agent: HashMap<String, (String, String, String)> = HashMap::new();
    for entry in &main.entries {
        let text = entry
            .get("stdout")
            .or_else(|| entry.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let agent_id = line_value(text, "agent_id")
            .or_else(|| xml_attribute(text, "agent_id"))
            .filter(|agent_id| is_kimi_agent_id(agent_id));
        let status = line_value(text, "status")
            .or_else(|| xml_attribute(text, "status"))
            .or_else(|| {
                text.contains("type=\"task.completed\"")
                    .then(|| "completed".to_string())
            })
            .or_else(|| {
                text.contains("type=\"task.failed\"")
                    .then(|| "failed".to_string())
            });
        let (Some(agent_id), Some(status)) = (agent_id, status) else {
            continue;
        };
        if !matches!(
            status.as_str(),
            "completed" | "success" | "failed" | "error" | "cancelled" | "canceled"
        ) {
            continue;
        }
        let ts = entry
            .get("ts")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let summary = text
            .split_once("[summary]")
            .map(|(_, summary)| summary.trim())
            .filter(|summary| !summary.is_empty())
            .map(compact_one_line)
            .unwrap_or_default();
        terminal_by_agent.insert(agent_id, (status, ts, summary));
    }
    for agent in agents.iter_mut().filter(|agent| agent.subagent) {
        let Some((status, ts, summary)) = terminal_by_agent.get(&agent.agent_id).cloned() else {
            continue;
        };
        let completed = matches!(status.as_str(), "completed" | "success");
        let mut content = if completed {
            "Task complete: Kimi subagent completed".to_string()
        } else {
            format!("Task ended: Kimi subagent {status}")
        };
        if !summary.is_empty() {
            content.push_str(": ");
            content.push_str(&summary);
        }
        let mut terminal = serde_json::json!({
            "ts": ts,
            "level": if completed { "info" } else { "warn" },
            "source": KIMI_SOURCE,
            "kind": "subagent_terminal",
            "content": content,
            "record_id": format!("{}:terminal", agent.agent_id),
            "agent_id": agent.agent_id,
            "parent_agent_id": agent.parent_agent_id,
            "subagent": true,
            "status": status,
        });
        if let Some(ts_ms) = terminal
            .get("ts")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_ms)
        {
            terminal["ts_ms"] = Value::from(ts_ms);
        }
        agent.updated_at = terminal
            .get("ts")
            .and_then(Value::as_str)
            .filter(|ts| !ts.is_empty())
            .map(str::to_string)
            .or_else(|| agent.updated_at.clone());
        agent.entries.push(terminal);
    }
}

fn line_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (candidate, value) = line.split_once(':')?;
        (candidate.trim() == key)
            .then(|| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn xml_attribute(text: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = text.find(&needle)? + needle.len();
    let end = text[start..].find('"')? + start;
    let value = text[start..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn compact_one_line(text: &str) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() > 240 {
        format!("{}…", compact.chars().take(240).collect::<String>())
    } else {
        compact
    }
}

fn read_session_index(kimi_home: &Path, record_limit: usize) -> Vec<(String, PathBuf)> {
    if record_limit == 0 {
        return Vec::new();
    }
    let index = kimi_home.join("session_index.jsonl");
    let mut read_record_budget = KIMI_INDEX_RECORD_LIMIT;
    let Ok((lines, _, _)) = read_bounded_complete_jsonl_lines(
        &index,
        KIMI_INDEX_READ_LIMIT,
        KIMI_INDEX_RECORD_READ_LIMIT,
        KIMI_INDEX_RECORD_LIMIT,
        &mut read_record_budget,
    ) else {
        return Vec::new();
    };
    // The index is append-only, so retain its newest valid records when the
    // caller asks for less than the bounded file can contain.
    let mut out = VecDeque::with_capacity(record_limit);
    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let Some(session_id) = value
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|id| is_kimi_session_id(id))
        else {
            continue;
        };
        let Some(session_dir) = value
            .get("sessionDir")
            .and_then(Value::as_str)
            .filter(|path| !path.trim().is_empty())
        else {
            continue;
        };
        if out.len() == record_limit {
            out.pop_front();
        }
        out.push_back((session_id.to_string(), PathBuf::from(session_dir)));
    }
    out.into_iter().collect()
}

fn safe_indexed_session_dir(
    sessions_root: &Path,
    indexed: &Path,
    expected_id: &str,
) -> Option<PathBuf> {
    let candidate = if indexed.is_absolute() {
        indexed.to_path_buf()
    } else {
        sessions_root.join(indexed)
    };
    if !candidate.is_dir() || session_id_from_dir(&candidate).as_deref() != Some(expected_id) {
        return None;
    }
    let root = std::fs::canonicalize(sessions_root).ok()?;
    let candidate = std::fs::canonicalize(candidate).ok()?;
    candidate.starts_with(&root).then_some(candidate)
}

fn collect_session_dirs(
    root: &Path,
    depth: usize,
    out: &mut Vec<PathBuf>,
    cap: usize,
    entry_budget: &mut usize,
) {
    if depth == 0 || out.len() >= cap || *entry_budget == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= cap || *entry_budget == 0 {
            break;
        }
        *entry_budget = (*entry_budget).saturating_sub(1);
        let path = entry.path();
        if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        if session_id_from_dir(&path).is_some()
            && path.join("state.json").is_file()
            && path.join("agents").is_dir()
        {
            out.push(path);
            continue;
        }
        collect_session_dirs(&path, depth - 1, out, cap, entry_budget);
    }
}

fn session_id_from_dir(path: &Path) -> Option<String> {
    let id = path.file_name()?.to_str()?;
    is_kimi_session_id(id).then(|| id.to_string())
}

fn parse_session_location(session_id: &str, session_dir: &Path) -> Option<KimiSessionLocation> {
    if !is_kimi_session_id(session_id) || has_parent_component(session_dir) {
        return None;
    }
    let canonical_session_dir = std::fs::canonicalize(session_dir).ok()?;
    let state_path = session_dir.join("state.json");
    let metadata = std::fs::metadata(&state_path).ok()?;
    if !metadata.is_file() || metadata.len() > KIMI_STATE_READ_LIMIT {
        return None;
    }
    if !std::fs::canonicalize(&state_path)
        .ok()?
        .starts_with(&canonical_session_dir)
    {
        return None;
    }
    let state_bytes = read_bounded_file(&state_path, KIMI_STATE_READ_LIMIT).ok()?;
    let state: Value = serde_json::from_slice(&state_bytes).ok()?;
    let agents_obj = state.get("agents").and_then(Value::as_object)?;
    let mut agent_ids = agents_obj
        .keys()
        .filter(|agent_id| is_kimi_agent_id(agent_id))
        .cloned()
        .collect::<Vec<_>>();
    agent_ids.sort_by(|left, right| {
        (left != KIMI_MAIN_AGENT)
            .cmp(&(right != KIMI_MAIN_AGENT))
            .then(left.cmp(right))
    });
    agent_ids.truncate(KIMI_AGENT_LIMIT);
    let mut agents = Vec::with_capacity(agent_ids.len());
    for agent_id in agent_ids {
        let agent_state = &agents_obj[&agent_id];
        let wire_path = session_dir
            .join("agents")
            .join(&agent_id)
            .join("wire.jsonl");
        if !wire_path.is_file()
            || !std::fs::canonicalize(&wire_path)
                .ok()
                .is_some_and(|wire| wire.starts_with(&canonical_session_dir))
        {
            continue;
        }
        agents.push(KimiAgentLocation {
            id: agent_id,
            parent_id: string_at(agent_state, "parentAgentId"),
            agent_type: string_at(agent_state, "type"),
            wire_path,
        });
    }
    agents.sort_by(|left, right| {
        (left.id != KIMI_MAIN_AGENT)
            .cmp(&(right.id != KIMI_MAIN_AGENT))
            .then(left.id.cmp(&right.id))
    });
    if !agents.iter().any(|agent| agent.id == KIMI_MAIN_AGENT) {
        return None;
    }
    Some(KimiSessionLocation {
        session_id: session_id.to_string(),
        session_dir: session_dir.to_path_buf(),
        state_path,
        created_at: string_at(&state, "createdAt"),
        updated_at: string_at(&state, "updatedAt"),
        title: string_at(&state, "title"),
        last_prompt: string_at(&state, "lastPrompt"),
        work_dir: string_at(&state, "workDir"),
        agents,
    })
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| component == Component::ParentDir)
}

fn read_complete_jsonl_lines(path: &Path) -> std::io::Result<(Vec<String>, u64)> {
    let mut record_budget = KIMI_WIRE_RECORD_LIMIT;
    read_bounded_complete_jsonl_lines(
        path,
        KIMI_WIRE_READ_LIMIT,
        KIMI_WIRE_RECORD_READ_LIMIT,
        KIMI_WIRE_RECORD_LIMIT,
        &mut record_budget,
    )
    .map(|(lines, consumed, _)| (lines, consumed))
}

fn read_bounded_file(path: &Path, limit: u64) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} exceeds the {limit} byte read limit", path.display()),
        ));
    }
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(usize::MAX)
            .min(usize::try_from(limit).unwrap_or(usize::MAX)),
    );
    Read::take(file, limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} grew beyond the {limit} byte read limit", path.display()),
        ));
    }
    Ok(bytes)
}

fn read_bounded_complete_jsonl_lines(
    path: &Path,
    file_limit: u64,
    record_limit: usize,
    record_count_limit: usize,
    work_record_budget: &mut usize,
) -> std::io::Result<(Vec<String>, u64, u64)> {
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > file_limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} exceeds the {file_limit} byte JSONL read limit",
                path.display()
            ),
        ));
    }
    let mut reader = std::io::BufReader::new(file);
    let mut lines = Vec::new();
    let mut consumed = 0u64;
    let mut read_total = 0u64;
    loop {
        if lines.len() == record_count_limit || *work_record_budget == 0 {
            if reader.fill_buf()?.is_empty() {
                break;
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} exceeds the {record_count_limit} record JSONL limit",
                    path.display()
                ),
            ));
        }
        let mut bytes = Vec::new();
        let read = BufRead::read_until(
            &mut Read::take(&mut reader, record_limit.saturating_add(1) as u64),
            b'\n',
            &mut bytes,
        )?;
        if read == 0 {
            break;
        }
        *work_record_budget = (*work_record_budget).saturating_sub(1);
        read_total = read_total.saturating_add(read as u64);
        if read_total > file_limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} grew beyond the {file_limit} byte JSONL read limit",
                    path.display()
                ),
            ));
        }
        if bytes.len() > record_limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{} contains a JSONL record larger than {record_limit} bytes",
                    path.display()
                ),
            ));
        }
        // Kimi appends its wire while a session is live. Never parse or
        // cursor past a torn trailing JSON object; the next sweep will pick
        // it up once its newline commits the record.
        if !bytes.ends_with(b"\n") {
            break;
        }
        let line = String::from_utf8(bytes).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} contains non-UTF-8 JSONL: {error}", path.display()),
            )
        })?;
        consumed = consumed.saturating_add(read as u64);
        lines.push(line);
    }
    Ok((lines, consumed, read_total))
}

fn next_active_turn(
    active_turns: &[TurnRef],
    latest_revision_by_turn: &mut HashMap<u32, u32>,
) -> TurnRef {
    let index = active_turns.len().saturating_add(1).min(u32::MAX as usize) as u32;
    let revision = latest_revision_by_turn
        .get(&index)
        .copied()
        .unwrap_or(0)
        .saturating_add(1);
    latest_revision_by_turn.insert(index, revision);
    TurnRef { index, revision }
}

/// Mirrors Kimi 0.27's `compactionUserMessageDisposition`: native undo
/// counts ordinary user prompts plus user-slash skill/plugin activations.
fn is_real_user_origin(origin: Option<&Value>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    match origin.get("kind").and_then(Value::as_str) {
        Some("user") | None => true,
        Some("skill_activation") | Some("plugin_command") => {
            origin.get("trigger").and_then(Value::as_str) == Some("user-slash")
        }
        _ => false,
    }
}

fn empty_agent_history(agent: &KimiAgentLocation) -> KimiAgentHistory {
    KimiAgentHistory {
        agent_id: agent.id.clone(),
        parent_agent_id: agent.parent_id.clone(),
        subagent: agent.subagent(),
        ..KimiAgentHistory::default()
    }
}

fn parse_agent_wire(agent: &KimiAgentLocation, work_record_budget: &mut usize) -> KimiAgentHistory {
    let mut history = empty_agent_history(agent);
    if *work_record_budget == 0 {
        return history;
    }
    let Ok((lines, consumed_bytes, _)) = read_bounded_complete_jsonl_lines(
        &agent.wire_path,
        KIMI_WIRE_READ_LIMIT,
        KIMI_WIRE_RECORD_READ_LIMIT,
        KIMI_WIRE_RECORD_LIMIT,
        work_record_budget,
    ) else {
        return history;
    };
    history.consumed_bytes = consumed_bytes;
    let has_prompt_lane = lines.iter().any(|line| {
        line.contains("\"turn.prompt\"")
            && serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|value| string_at(&value, "type"))
                .as_deref()
                == Some("turn.prompt")
    });
    let mut current_turn: Option<TurnRef> = None;
    let mut active_turns = Vec::<TurnRef>::new();
    let mut latest_revision_by_turn = HashMap::<u32, u32>::new();
    let mut undo_floor = 0usize;
    let mut context_generation = 0u64;

    for (zero_line, line) in lines.iter().enumerate() {
        let line_no = zero_line as u64 + 1;
        let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        let ts_ms = value
            .get("time")
            .or_else(|| value.get("created_at"))
            .and_then(Value::as_i64)
            .or_else(|| {
                value
                    .get("time")
                    .or_else(|| value.get("created_at"))
                    .and_then(Value::as_u64)
                    .and_then(|value| i64::try_from(value).ok())
            });
        let ts = ts_ms.and_then(timestamp_from_ms).unwrap_or_default();
        if history.created_at.is_none() && event_type == "metadata" {
            history.created_at = ts_ms.and_then(timestamp_from_ms);
        }
        if let Some(ts) = (!ts.is_empty()).then_some(ts.clone()) {
            history.updated_at = Some(ts);
        }

        match event_type {
            "config.update" => {
                if let Some(model) = string_at(&value, "modelAlias") {
                    history.model = Some(model);
                }
            }
            "turn.prompt" => {
                let Some(text) = input_text(value.get("input")) else {
                    continue;
                };
                let real_user = is_real_user_origin(value.get("origin"));
                // Kimi's native undo count is defined over real-user
                // prompts. System-trigger prompts (autonomy/subagent
                // bootstrap) remain visible but must not skew rewind or
                // anchor ordinals.
                let turn = if real_user {
                    let turn = next_active_turn(&active_turns, &mut latest_revision_by_turn);
                    current_turn = Some(turn);
                    active_turns.push(turn);
                    history.turns = history.turns.saturating_add(1);
                    Some(turn)
                } else {
                    current_turn = None;
                    None
                };
                if history.first_prompt.is_none() {
                    history.first_prompt = Some(text.clone());
                }
                let mut entry = message_entry(
                    &history,
                    &ts,
                    "user",
                    text,
                    record_id_for(&history.agent_id, &value, line_no),
                    turn,
                );
                entry["origin"] = value.get("origin").cloned().unwrap_or(Value::Null);
                if !real_user {
                    entry["system_trigger"] = Value::Bool(true);
                }
                history.entries.push(entry);
            }
            "turn.steer" => {
                let Some(text) = input_text(value.get("input")) else {
                    continue;
                };
                let origin = string_pointer(&value, "/origin/kind");
                if origin.as_deref() == Some("user") {
                    let mut entry = message_entry(
                        &history,
                        &ts,
                        "user",
                        text,
                        record_id_for(&history.agent_id, &value, line_no),
                        current_turn,
                    );
                    entry["kind"] = Value::String("steer".to_string());
                    entry["mid_turn"] = Value::Bool(true);
                    entry["origin"] = value.get("origin").cloned().unwrap_or(Value::Null);
                    history.entries.push(entry);
                } else {
                    history.entries.push(system_entry(
                        &history,
                        &ts,
                        "steer",
                        text,
                        record_id_for(&history.agent_id, &value, line_no),
                        current_turn,
                    ));
                }
            }
            "turn.cancel" => {
                history.entries.push(system_entry(
                    &history,
                    &ts,
                    "turn_cancelled",
                    "Turn cancelled.".to_string(),
                    record_id_for(&history.agent_id, &value, line_no),
                    current_turn,
                ));
                current_turn = None;
            }
            "context.apply_compaction" => {
                undo_floor = active_turns.len();
                context_generation = context_generation.saturating_add(1);
                history.has_undo_boundary = true;
                let summary = string_at(&value, "contextSummary")
                    .or_else(|| string_at(&value, "summary"))
                    .unwrap_or_else(|| {
                        "Context compacted; earlier turns remain in transcript history.".to_string()
                    });
                let mut marker = system_entry(
                    &history,
                    &ts,
                    "compaction",
                    summary,
                    record_id_for(&history.agent_id, &value, line_no),
                    None,
                );
                marker["undo_floor_turn"] = Value::from(undo_floor as u64);
                if let Some(compacted_count) = value.get("compactedCount").and_then(Value::as_u64) {
                    marker["compacted_count"] = Value::from(compacted_count);
                }
                history.entries.push(marker);
                current_turn = None;
            }
            "context.clear" => {
                undo_floor = active_turns.len();
                context_generation = context_generation.saturating_add(1);
                history.has_undo_boundary = true;
                let mut marker = system_entry(
                    &history,
                    &ts,
                    "context_clear",
                    "Context cleared; earlier turns remain in transcript history.".to_string(),
                    record_id_for(&history.agent_id, &value, line_no),
                    None,
                );
                marker["undo_floor_turn"] = Value::from(undo_floor as u64);
                history.entries.push(marker);
                current_turn = None;
            }
            "context.undo" => {
                let count = value.get("count").and_then(Value::as_u64).unwrap_or(1);
                let mut removed = Vec::new();
                for _ in 0..count {
                    if active_turns.len() <= undo_floor {
                        break;
                    }
                    let Some(turn) = active_turns.pop() else {
                        break;
                    };
                    removed.push(turn);
                }
                mark_turns_superseded(&mut history.entries, &removed, &ts);
                if !removed.is_empty() {
                    let mut marker = system_entry(
                        &history,
                        &ts,
                        "rollback_marker",
                        if removed.len() == 1 {
                            "Rewound 1 user turn; overwritten entries are no longer active context."
                                .to_string()
                        } else {
                            format!(
                                "Rewound {} user turns; overwritten entries are no longer active context.",
                                removed.len()
                            )
                        },
                        record_id_for(&history.agent_id, &value, line_no),
                        current_turn,
                    );
                    marker["rollback_turns"] = Value::from(removed.len() as u64);
                    marker["turns_removed"] = Value::from(removed.len() as u64);
                    marker["removed_turn_ids"] = Value::Array(
                        removed
                            .iter()
                            .map(|turn| {
                                Value::String(format!("turn-{}-r{}", turn.index, turn.revision))
                            })
                            .collect(),
                    );
                    history.entries.push(marker);
                }
                current_turn = active_turns.last().copied();
            }
            "context.append_message" if !has_prompt_lane => {
                let role = string_pointer(&value, "/message/role");
                if role.as_deref() != Some("user") {
                    continue;
                }
                let Some(text) = input_text(value.pointer("/message/content")) else {
                    continue;
                };
                let real_user = is_real_user_origin(value.pointer("/message/origin"));
                let turn = if real_user {
                    let turn = next_active_turn(&active_turns, &mut latest_revision_by_turn);
                    current_turn = Some(turn);
                    active_turns.push(turn);
                    history.turns = history.turns.saturating_add(1);
                    Some(turn)
                } else {
                    current_turn = None;
                    None
                };
                history.first_prompt.get_or_insert_with(|| text.clone());
                let mut entry = message_entry(
                    &history,
                    &ts,
                    "user",
                    text,
                    record_id_for(&history.agent_id, &value, line_no),
                    turn,
                );
                entry["origin"] = value
                    .pointer("/message/origin")
                    .cloned()
                    .unwrap_or(Value::Null);
                if !real_user {
                    entry["system_trigger"] = Value::Bool(true);
                }
                history.entries.push(entry);
            }
            "context.append_loop_event" => {
                let Some(event) = value.get("event") else {
                    continue;
                };
                match event.get("type").and_then(Value::as_str).unwrap_or("") {
                    "content.part" => {
                        let Some(part) = event.get("part") else {
                            continue;
                        };
                        let (kind, role, text) =
                            match part.get("type").and_then(Value::as_str).unwrap_or("") {
                                "text" => (
                                    "message",
                                    "assistant",
                                    string_at(part, "text").unwrap_or_default(),
                                ),
                                "think" | "thinking" => (
                                    "reasoning",
                                    "assistant",
                                    string_at(part, "think")
                                        .or_else(|| string_at(part, "thinking"))
                                        .unwrap_or_default(),
                                ),
                                _ => continue,
                            };
                        if text.trim().is_empty() {
                            continue;
                        }
                        let mut entry = message_entry(
                            &history,
                            &ts,
                            role,
                            text,
                            record_id_for(&history.agent_id, event, line_no),
                            current_turn,
                        );
                        if kind != "message" {
                            entry["kind"] = Value::String(kind.to_string());
                        }
                        history.entries.push(entry);
                    }
                    "tool.call" => {
                        let name = string_at(event, "name").unwrap_or_else(|| "tool".to_string());
                        let content = string_at(event, "description")
                            .unwrap_or_else(|| format!("Calling {name}"));
                        let mut entry = agent_entry(
                            &history,
                            &ts,
                            "tool_call",
                            content,
                            record_id_for(&history.agent_id, event, line_no),
                            current_turn,
                        );
                        entry["tool_name"] = Value::String(name);
                        if let Some(id) =
                            string_at(event, "toolCallId").or_else(|| string_at(event, "uuid"))
                        {
                            entry["tool_call_id"] = Value::String(id);
                        }
                        if let Some(args) = event.get("args") {
                            entry["tool_args"] = args.clone();
                        }
                        if let Some(display) = event.get("display") {
                            entry["tool_display"] = display.clone();
                        }
                        history.entries.push(entry);
                    }
                    "tool.result" => {
                        let result = event.get("result").cloned().unwrap_or(Value::Null);
                        let output = result
                            .get("output")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let is_error = result
                            .get("isError")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let mut entry = agent_entry(
                            &history,
                            &ts,
                            "agent_output",
                            if is_error {
                                "Tool failed.".to_string()
                            } else {
                                "Tool completed.".to_string()
                            },
                            record_id_for(&history.agent_id, event, line_no),
                            current_turn,
                        );
                        entry["event"] = Value::String("agent_output".to_string());
                        entry["stdout"] = Value::String(output);
                        entry["stderr"] = Value::String(String::new());
                        entry["is_error"] = Value::Bool(is_error);
                        if let Some(id) = string_at(event, "toolCallId")
                            .or_else(|| string_at(event, "parentUuid"))
                        {
                            entry["tool_call_id"] = Value::String(id);
                        }
                        history.entries.push(entry);
                    }
                    _ => {}
                }
            }
            "usage.record" => {
                if let Some(parsed) = usage_from_value(value.get("usage")) {
                    history.usage.add(parsed);
                    if let Some(day) = ts.get(0..10).filter(|day| valid_day(day)) {
                        history
                            .daily_usage
                            .entry(day.to_string())
                            .or_default()
                            .add(parsed);
                    }
                }
                if history.model.is_none() {
                    history.model = string_at(&value, "model");
                }
            }
            "permission.record_approval_result" => {
                let action =
                    string_at(&value, "action").unwrap_or_else(|| "Tool approval".to_string());
                let decision = string_pointer(&value, "/result/decision")
                    .unwrap_or_else(|| "recorded".to_string());
                history.entries.push(system_entry(
                    &history,
                    &ts,
                    "approval",
                    format!("{action}: {decision}"),
                    record_id_for(&history.agent_id, &value, line_no),
                    current_turn,
                ));
            }
            _ => {}
        }
    }
    history.active_real_user_turns = active_turns.len().min(u32::MAX as usize) as u32;
    history.undoable_real_user_turns = active_turns
        .len()
        .saturating_sub(undo_floor)
        .min(u32::MAX as usize) as u32;
    history.active_turn_revisions = active_turns.iter().map(|turn| turn.revision).collect();
    history.undo_floor = undo_floor.min(u32::MAX as usize) as u32;
    history.context_generation = context_generation;
    history.head_fingerprint =
        active_turn_fingerprint(&active_turns, undo_floor, context_generation);
    history
}

fn active_turn_fingerprint(
    active_turns: &[TurnRef],
    undo_floor: usize,
    context_generation: u64,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"kimi-head-v1\0");
    digest.update((undo_floor as u64).to_le_bytes());
    digest.update(context_generation.to_le_bytes());
    for turn in active_turns {
        digest.update(turn.index.to_le_bytes());
        digest.update(turn.revision.to_le_bytes());
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn record_id_for(agent_id: &str, value: &Value, line_no: u64) -> String {
    let native = string_at(value, "uuid")
        .or_else(|| string_at(value, "toolCallId"))
        .or_else(|| string_at(value, "messageId"))
        .unwrap_or_else(|| format!("line-{line_no}"));
    format!("{agent_id}:{native}")
}

fn common_entry(
    history: &KimiAgentHistory,
    ts: &str,
    record_id: String,
    turn: Option<TurnRef>,
) -> Value {
    let mut value = serde_json::json!({
        "ts": ts,
        "record_id": record_id,
        "agent_id": history.agent_id,
        "subagent": history.subagent,
    });
    if let Some(parent) = history.parent_agent_id.as_deref() {
        value["parent_agent_id"] = Value::String(parent.to_string());
    }
    if let Some(turn) = turn {
        value["user_turn_index"] = Value::from(turn.index);
        value["user_turn_revision"] = Value::from(turn.revision);
        value["turn_id"] = Value::String(format!("turn-{}-r{}", turn.index, turn.revision));
    }
    value
}

fn message_entry(
    history: &KimiAgentHistory,
    ts: &str,
    role: &str,
    content: String,
    record_id: String,
    turn: Option<TurnRef>,
) -> Value {
    let mut value = common_entry(history, ts, record_id, turn);
    value["level"] = Value::String(if role == "assistant" {
        "model".to_string()
    } else {
        "info".to_string()
    });
    value["source"] = Value::String(if role == "user" {
        "user".to_string()
    } else {
        KIMI_SOURCE.to_string()
    });
    value["role"] = Value::String(role.to_string());
    value["content"] = Value::String(content);
    value
}

fn system_entry(
    history: &KimiAgentHistory,
    ts: &str,
    kind: &str,
    content: String,
    record_id: String,
    turn: Option<TurnRef>,
) -> Value {
    let mut value = common_entry(history, ts, record_id, turn);
    value["level"] = Value::String(if kind == "turn_cancelled" || kind == "rollback_marker" {
        "warn".to_string()
    } else {
        "info".to_string()
    });
    value["source"] = Value::String("system".to_string());
    value["kind"] = Value::String(kind.to_string());
    value["content"] = Value::String(content);
    value
}

fn agent_entry(
    history: &KimiAgentHistory,
    ts: &str,
    kind: &str,
    content: String,
    record_id: String,
    turn: Option<TurnRef>,
) -> Value {
    let mut value = common_entry(history, ts, record_id, turn);
    value["level"] = Value::String("agent".to_string());
    value["source"] = Value::String(KIMI_SOURCE.to_string());
    value["kind"] = Value::String(kind.to_string());
    value["content"] = Value::String(content);
    value
}

fn mark_turns_superseded(entries: &mut [Value], turns: &[TurnRef], ts: &str) {
    if turns.is_empty() {
        return;
    }
    for entry in entries {
        let Some(turn_index) = entry
            .get("user_turn_index")
            .and_then(Value::as_u64)
            .and_then(|turn| u32::try_from(turn).ok())
        else {
            continue;
        };
        let turn_revision = entry
            .get("user_turn_revision")
            .and_then(Value::as_u64)
            .and_then(|revision| u32::try_from(revision).ok())
            .unwrap_or(1);
        if !turns.contains(&TurnRef {
            index: turn_index,
            revision: turn_revision,
        }) {
            continue;
        }
        entry["superseded"] = Value::Bool(true);
        entry["superseded_at"] = Value::String(ts.to_string());
        entry["superseded_reason"] = Value::String("context_undo".to_string());
    }
}

fn usage_from_value(value: Option<&Value>) -> Option<KimiUsage> {
    let value = value?;
    Some(KimiUsage {
        input_other: value.get("inputOther").and_then(Value::as_u64).unwrap_or(0),
        output: value.get("output").and_then(Value::as_u64).unwrap_or(0),
        cache_read: value
            .get("inputCacheRead")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_creation: value
            .get("inputCacheCreation")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

fn input_text(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if let Some(text) = value.as_str() {
        return (!text.trim().is_empty()).then(|| text.to_string());
    }
    let parts = value.as_array()?;
    let mut texts = Vec::new();
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        if let Some(text) = part
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
        {
            texts.push(text);
        }
    }
    (!texts.is_empty()).then(|| texts.join("\n"))
}

fn string_at(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn string_pointer(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn timestamp_from_ms(ms: i64) -> Option<String> {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

fn parse_timestamp_ms(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

fn valid_day(day: &str) -> bool {
    day.len() == 10
        && day.as_bytes()[4] == b'-'
        && day.as_bytes()[7] == b'-'
        && day
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

fn path_activity_mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn session_candidate_activity_mtime(session_dir: &Path) -> i64 {
    let agents_dir = session_dir.join("agents");
    let mut newest = path_activity_mtime(session_dir)
        .max(path_activity_mtime(&session_dir.join("state.json")))
        .max(path_activity_mtime(&agents_dir));
    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return newest;
    };
    for entry in entries.flatten().take(KIMI_AGENT_LIMIT) {
        if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let dir = entry.path();
        newest = newest
            .max(path_activity_mtime(&dir))
            .max(path_activity_mtime(&dir.join("wire.jsonl")));
    }
    newest
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "session_01234567-89ab-cdef-0123-456789abcdef";

    fn write_fixture(home: &Path) -> PathBuf {
        let session_dir = home
            .join(KIMI_HOME_DIR)
            .join("sessions")
            .join("wd_repo_abc")
            .join(SESSION);
        std::fs::create_dir_all(session_dir.join("agents/main")).unwrap();
        std::fs::create_dir_all(session_dir.join("agents/agent-0")).unwrap();
        std::fs::write(
            session_dir.join("state.json"),
            serde_json::json!({
                "createdAt": "2026-07-19T10:00:00.000Z",
                "updatedAt": "2026-07-19T10:10:00.000Z",
                "title": "Inspect the parser",
                "lastPrompt": "replacement prompt",
                "workDir": "/repo",
                "agents": {
                    "main": {
                        "homedir": session_dir.join("agents/main"),
                        "type": "main",
                        "parentAgentId": null
                    },
                    "agent-0": {
                        "homedir": session_dir.join("agents/agent-0"),
                        "type": "sub",
                        "parentAgentId": "main"
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let main = [
            serde_json::json!({"type":"metadata","protocol_version":"1.4","created_at":1784455200000i64}),
            serde_json::json!({"type":"config.update","modelAlias":"kimi-code/k2.7-coding","time":1784455200001i64}),
            serde_json::json!({"type":"turn.prompt","input":[{"type":"text","text":"old prompt"}],"origin":{"kind":"user"},"time":1784455201000i64}),
            serde_json::json!({"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"old prompt"}],"origin":{"kind":"user"}},"time":1784455201001i64}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"content.part","uuid":"think-1","part":{"type":"think","think":"old reasoning"}},"time":1784455202000i64}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"content.part","uuid":"text-1","part":{"type":"text","text":"old answer"}},"time":1784455203000i64}),
            serde_json::json!({"type":"turn.cancel","time":1784455203500i64}),
            serde_json::json!({"type":"context.undo","count":1,"time":1784455204000i64}),
            serde_json::json!({"type":"turn.prompt","input":[{"type":"text","text":"replacement prompt"}],"origin":{"kind":"user"},"time":1784455205000i64}),
            serde_json::json!({"type":"turn.steer","input":[{"type":"text","text":"also inspect usage"}],"origin":{"kind":"user"},"time":1784455205500i64}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.call","uuid":"tool-1","toolCallId":"tool-1","name":"Read","description":"Reading state.json","args":{"path":"state.json"}},"time":1784455206000i64}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.result","parentUuid":"tool-1","toolCallId":"tool-1","result":{"output":"ok"}},"time":1784455207000i64}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"content.part","uuid":"text-2","part":{"type":"text","text":"new answer"}},"time":1784455208000i64}),
            serde_json::json!({"type":"usage.record","model":"kimi-code/k2.7-coding","usage":{"inputOther":10,"output":3,"inputCacheRead":20,"inputCacheCreation":4},"usageScope":"turn","time":1784455209000i64}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"tool.result","parentUuid":"agent-tool","toolCallId":"agent-tool","result":{"output":"agent_id: agent-0\nstatus: completed\n\n[summary]\nCHILD_OK"}},"time":1784455210000i64}),
        ];
        std::fs::write(
            session_dir.join("agents/main/wire.jsonl"),
            main.iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        let child = [
            serde_json::json!({"type":"metadata","protocol_version":"1.4","created_at":1784455206100i64}),
            serde_json::json!({"type":"turn.prompt","input":[{"type":"text","text":"child prompt"}],"origin":{"kind":"system_trigger","name":"subagent"},"time":1784455206200i64}),
            serde_json::json!({"type":"context.append_loop_event","event":{"type":"content.part","uuid":"child-text","part":{"type":"text","text":"child answer"}},"time":1784455206300i64}),
            serde_json::json!({"type":"usage.record","model":"kimi-code/k2.7-coding","usage":{"inputOther":2,"output":1,"inputCacheRead":5,"inputCacheCreation":0},"usageScope":"turn","time":1784455206400i64}),
        ];
        std::fs::write(
            session_dir.join("agents/agent-0/wire.jsonl"),
            child
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        let index = home.join(KIMI_HOME_DIR).join("session_index.jsonl");
        std::fs::write(
            index,
            serde_json::json!({
                "sessionId": SESSION,
                "sessionDir": session_dir,
                "workDir": "/repo"
            })
            .to_string()
                + "\n",
        )
        .unwrap();
        session_dir
    }

    #[test]
    fn scans_real_shape_and_parses_history() {
        let home = tempfile::tempdir().unwrap();
        write_fixture(home.path());

        let locations = list_kimi_sessions_in(&home.path().join(KIMI_HOME_DIR), 10);
        assert_eq!(locations.len(), 1);
        let history = parse_kimi_session(locations[0].clone());
        let main = history.selected_agent(SESSION).unwrap();
        assert_eq!(main.model.as_deref(), Some("kimi-code/k2.7-coding"));
        assert_eq!(main.turns, 2);
        assert_eq!(
            main.usage,
            KimiUsage {
                input_other: 10,
                output: 3,
                cache_read: 20,
                cache_creation: 4,
            }
        );
        assert!(main
            .entries
            .iter()
            .any(|entry| { entry.get("kind").and_then(Value::as_str) == Some("rollback_marker") }));
        assert!(main.entries.iter().any(|entry| {
            entry.get("content").and_then(Value::as_str) == Some("old answer")
                && entry.get("superseded").and_then(Value::as_bool) == Some(true)
        }));
        assert!(main.entries.iter().any(|entry| {
            entry.get("content").and_then(Value::as_str) == Some("also inspect usage")
                && entry.get("kind").and_then(Value::as_str) == Some("steer")
        }));
        let replacement = main
            .entries
            .iter()
            .find(|entry| {
                entry.get("content").and_then(Value::as_str) == Some("replacement prompt")
            })
            .unwrap();
        assert_eq!(replacement["user_turn_index"], 1);
        assert_eq!(replacement["user_turn_revision"], 2);
        // `context.append_message` is the persisted twin of turn.prompt,
        // not a second user turn.
        assert_eq!(
            main.entries
                .iter()
                .filter(|entry| {
                    entry.get("content").and_then(Value::as_str) == Some("old prompt")
                })
                .count(),
            1
        );

        let child_id = kimi_child_session_id(SESSION, "agent-0").unwrap();
        let child = history.selected_agent(&child_id).unwrap();
        assert!(child.subagent);
        assert_eq!(child.parent_agent_id.as_deref(), Some("main"));
        assert_eq!(child.first_prompt.as_deref(), Some("child prompt"));
        assert!(child.entries.iter().any(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some("subagent_terminal")
                && entry
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| content.contains("CHILD_OK"))
        }));

        let state = kimi_user_turn_state_from_history(home.path(), SESSION).unwrap();
        assert_eq!(state.active_count(), 1);
        assert_eq!(state.active_revision(1), Some(2));
    }

    #[test]
    fn persisted_bridge_root_drives_list_find_and_horizon() {
        let user_home = tempfile::tempdir().unwrap();
        let fixture_home = tempfile::tempdir().unwrap();
        let expected_session_dir = write_fixture(fixture_home.path());
        let bridge = fixture_home.path().join(KIMI_HOME_DIR);
        let log_dir = user_home
            .path()
            .join(".intendant")
            .join("logs")
            .join("kimi-wrapper");
        let config = crate::session_config::SessionAgentConfig {
            source: Some(KIMI_SOURCE.to_string()),
            kimi_home: Some(bridge.to_string_lossy().to_string()),
            ..Default::default()
        };
        crate::session_config::write_log_dir_config(&log_dir, &config).unwrap();

        assert_eq!(list_kimi_sessions_from_home(user_home.path(), 10).len(), 1);
        let found = find_kimi_session_from_home(user_home.path(), SESSION).unwrap();
        assert_eq!(
            std::fs::canonicalize(found.session_dir).unwrap(),
            std::fs::canonicalize(expected_session_dir).unwrap()
        );
        assert_eq!(
            kimi_turn_horizon_from_history(user_home.path(), SESSION)
                .unwrap()
                .active_turns,
            1
        );
    }

    #[test]
    fn find_chooses_newest_duplicate_across_bridge_and_primary_roots() {
        let user_home = tempfile::tempdir().unwrap();
        let fixture_home = tempfile::tempdir().unwrap();
        let stale_session_dir = write_fixture(fixture_home.path());
        let bridge = fixture_home.path().join(KIMI_HOME_DIR);
        let log_dir = user_home
            .path()
            .join(".intendant")
            .join("logs")
            .join("kimi-wrapper");
        let config = crate::session_config::SessionAgentConfig {
            source: Some(KIMI_SOURCE.to_string()),
            kimi_home: Some(bridge.to_string_lossy().to_string()),
            ..Default::default()
        };
        crate::session_config::write_log_dir_config(&log_dir, &config).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(25));
        let current_session_dir = write_fixture(user_home.path());
        std::fs::write(current_session_dir.join("new-activity"), "current").unwrap();
        assert!(
            path_activity_mtime(&current_session_dir) > path_activity_mtime(&stale_session_dir),
            "fixture must establish a strictly newer primary copy"
        );

        let found = find_kimi_session_from_home(user_home.path(), SESSION).unwrap();
        assert_eq!(
            std::fs::canonicalize(found.session_dir).unwrap(),
            std::fs::canonicalize(current_session_dir).unwrap()
        );
    }

    #[test]
    fn index_escape_is_rejected_but_bounded_walk_recovers_real_session() {
        let home = tempfile::tempdir().unwrap();
        let session_dir = write_fixture(home.path());
        let outside = tempfile::tempdir().unwrap();
        let fake = outside.path().join(SESSION);
        std::fs::create_dir_all(fake.join("agents/main")).unwrap();
        std::fs::write(fake.join("state.json"), "{}").unwrap();
        std::fs::write(fake.join("agents/main/wire.jsonl"), "").unwrap();
        std::fs::write(
            home.path().join(KIMI_HOME_DIR).join("session_index.jsonl"),
            serde_json::json!({
                "sessionId": SESSION,
                "sessionDir": fake,
                "workDir": "/evil"
            })
            .to_string(),
        )
        .unwrap();

        let found = find_kimi_session_in(&home.path().join(KIMI_HOME_DIR), SESSION).unwrap();
        assert_eq!(
            std::fs::canonicalize(found.session_dir).unwrap(),
            std::fs::canonicalize(session_dir).unwrap()
        );
    }

    #[test]
    fn compaction_sets_a_hard_undo_floor_and_post_floor_revisions_replay() {
        use std::io::Write;

        let home = tempfile::tempdir().unwrap();
        let session_dir = write_fixture(home.path());
        let mut wire = std::fs::OpenOptions::new()
            .append(true)
            .open(session_dir.join("agents/main/wire.jsonl"))
            .unwrap();
        for record in [
            serde_json::json!({
                "type":"context.apply_compaction",
                "summary":"summary",
                "contextSummary":"compacted context",
                "compactedCount":12,
                "keptUserMessageCount":1,
                "time":1784455211000i64
            }),
            serde_json::json!({
                "type":"turn.prompt",
                "input":[{"type":"text","text":"post-compaction prompt"}],
                "origin":{"kind":"user"},
                "time":1784455212000i64
            }),
            serde_json::json!({
                "type":"context.undo","count":1,"time":1784455213000i64
            }),
            serde_json::json!({
                "type":"turn.prompt",
                "input":[{"type":"text","text":"post-compaction replacement"}],
                "origin":{"kind":"user"},
                "time":1784455214000i64
            }),
        ] {
            writeln!(wire, "{record}").unwrap();
        }
        drop(wire);

        let location = find_kimi_session_in(&home.path().join(KIMI_HOME_DIR), SESSION).unwrap();
        let history = parse_kimi_session(location);
        let main = history.selected_agent(SESSION).unwrap();
        assert!(main.has_undo_boundary);
        assert_eq!(main.undoable_real_user_turns, 1);
        let replacement = main
            .entries
            .iter()
            .find(|entry| {
                entry.get("content").and_then(Value::as_str) == Some("post-compaction replacement")
            })
            .unwrap();
        assert_eq!(replacement["user_turn_index"], 2);
        assert_eq!(replacement["user_turn_revision"], 2);
        assert!(main
            .entries
            .iter()
            .any(|entry| entry.get("kind").and_then(Value::as_str) == Some("compaction")));

        let state = kimi_user_turn_state_from_history(home.path(), SESSION).unwrap();
        assert_eq!(state.active_count(), 2);
        assert_eq!(state.active_revision(2), Some(2));
        let horizon = kimi_turn_horizon_from_history(home.path(), SESSION).unwrap();
        assert_eq!(horizon.active_turns, 2);
        assert_eq!(horizon.undoable_turns, 1);
        assert!(horizon.has_boundary);
        assert_eq!(horizon.head_fingerprint.len(), 64);

        let rollback_target = horizon.after_rollback(1).expect("reachable target");
        assert_eq!(rollback_target.active_turns, 1);
        assert_eq!(rollback_target.undoable_turns, 0);
        assert!(horizon.after_rollback(2).is_none());
        let mut wire = std::fs::OpenOptions::new()
            .append(true)
            .open(session_dir.join("agents/main/wire.jsonl"))
            .unwrap();
        writeln!(
            wire,
            "{}",
            serde_json::json!({
                "type":"context.undo","count":1,"time":1784455215000i64
            })
        )
        .unwrap();
        drop(wire);
        let actual =
            kimi_turn_horizon_from_history(home.path(), SESSION).expect("post-undo horizon");
        assert_eq!(actual, rollback_target);
    }

    #[test]
    fn identity_shape_is_canonical_and_composite_children_are_unambiguous() {
        assert!(is_kimi_session_id(SESSION));
        assert!(!is_kimi_session_id("../session_bad"));
        assert_eq!(
            split_kimi_session_id(&format!("{SESSION}:agent-12")),
            Some((SESSION, Some("agent-12")))
        );
        assert_eq!(split_kimi_session_id("session_bad:../agent"), None);
        assert_eq!(kimi_child_session_id(SESSION, "main"), None);
    }

    #[test]
    fn native_real_user_origin_policy_includes_only_user_slash_activations() {
        assert!(is_real_user_origin(None));
        assert!(is_real_user_origin(Some(
            &serde_json::json!({"kind":"user"})
        )));
        assert!(is_real_user_origin(Some(
            &serde_json::json!({"kind":"skill_activation","trigger":"user-slash"})
        )));
        assert!(is_real_user_origin(Some(
            &serde_json::json!({"kind":"plugin_command","trigger":"user-slash"})
        )));
        assert!(!is_real_user_origin(Some(
            &serde_json::json!({"kind":"skill_activation","trigger":"system"})
        )));
        assert!(!is_real_user_origin(Some(
            &serde_json::json!({"kind":"system_trigger"})
        )));
    }

    #[test]
    fn legacy_horizon_without_rollback_proof_remains_head_comparable() {
        let legacy: KimiTurnHorizon = serde_json::from_value(serde_json::json!({
            "active_turns": 2,
            "undoable_turns": 1,
            "has_boundary": true,
            "head_fingerprint": "legacy-head"
        }))
        .unwrap();
        let with_proof = KimiTurnHorizon {
            active_turns: 2,
            undoable_turns: 1,
            has_boundary: true,
            head_fingerprint: "legacy-head".to_string(),
            rollback_proof: Some(KimiRollbackProof {
                active_revisions: vec![1, 1],
                undo_floor: 1,
                context_generation: 1,
            }),
        };
        assert_eq!(legacy, with_proof);
        assert!(legacy.after_rollback(1).is_none());
    }

    #[test]
    fn jsonl_reader_bounds_records_total_bytes_and_torn_tails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire.jsonl");

        std::fs::write(&path, b"{}\n{\"type\":\"torn\"").unwrap();
        let (lines, consumed) = read_complete_jsonl_lines(&path).unwrap();
        assert_eq!(lines, vec!["{}\n"]);
        assert_eq!(consumed, 3);

        let mut oversized_record = vec![b' '; KIMI_WIRE_RECORD_READ_LIMIT + 1];
        oversized_record.push(b'\n');
        std::fs::write(&path, oversized_record).unwrap();
        let error = read_complete_jsonl_lines(&path).unwrap_err();
        assert!(error.to_string().contains("record larger"));

        let file = std::fs::File::create(&path).unwrap();
        file.set_len(KIMI_WIRE_READ_LIMIT + 1).unwrap();
        let error = read_complete_jsonl_lines(&path).unwrap_err();
        assert!(error.to_string().contains("JSONL read limit"));
    }

    #[test]
    fn shared_jsonl_record_budget_stops_adversarial_many_record_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wire.jsonl");
        std::fs::write(&path, b"{}\n{}\n").unwrap();
        let mut work_budget = 1;
        let error =
            read_bounded_complete_jsonl_lines(&path, 1_024, 128, 10, &mut work_budget).unwrap_err();
        assert_eq!(work_budget, 0);
        assert!(error.to_string().contains("record JSONL limit"));
    }

    #[test]
    fn oversized_wire_fails_closed_without_partial_hydration() {
        let home = tempfile::tempdir().unwrap();
        let session_dir = write_fixture(home.path());
        let main_wire = session_dir.join("agents/main/wire.jsonl");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&main_wire)
            .unwrap()
            .set_len(KIMI_WIRE_READ_LIMIT + 1)
            .unwrap();

        let location = find_kimi_session_in(&home.path().join(KIMI_HOME_DIR), SESSION).unwrap();
        let parsed = parse_kimi_session(location);
        let main = parsed.selected_agent(SESSION).unwrap();
        assert_eq!(main.consumed_bytes, 0);
        assert!(main.entries.is_empty());
        assert_eq!(main.usage, KimiUsage::default());
    }

    #[test]
    fn oversized_index_record_falls_back_to_the_bounded_session_walk() {
        let home = tempfile::tempdir().unwrap();
        let expected = write_fixture(home.path());
        let index = home.path().join(KIMI_HOME_DIR).join("session_index.jsonl");
        let mut oversized = vec![b'x'; KIMI_INDEX_RECORD_READ_LIMIT + 1];
        oversized.push(b'\n');
        std::fs::write(index, oversized).unwrap();

        let found = find_kimi_session_in(&home.path().join(KIMI_HOME_DIR), SESSION).unwrap();
        assert_eq!(
            std::fs::canonicalize(found.session_dir).unwrap(),
            std::fs::canonicalize(expected).unwrap()
        );
    }

    #[test]
    fn fallback_find_prefers_the_newest_duplicate_within_one_home() {
        let home = tempfile::tempdir().unwrap();
        let kimi_home = home.path().join(KIMI_HOME_DIR);
        let make = |key: &str, marker: &str| {
            let dir = kimi_home.join("sessions").join(key).join(SESSION);
            std::fs::create_dir_all(dir.join("agents/main")).unwrap();
            std::fs::write(
                dir.join("state.json"),
                serde_json::json!({
                    "workDir": format!("/{marker}"),
                    "agents": {"main": {"type": "main", "parentAgentId": null}}
                })
                .to_string(),
            )
            .unwrap();
            std::fs::write(dir.join("agents/main/wire.jsonl"), "{}\n").unwrap();
            dir
        };
        let current = make("wd_current", "current");
        std::thread::sleep(std::time::Duration::from_millis(25));
        let stale = make("wd_stale", "stale");
        std::thread::sleep(std::time::Duration::from_millis(25));
        std::fs::write(current.join("agents/main/wire.jsonl"), "{}\n{}\n").unwrap();
        assert!(
            session_candidate_activity_mtime(&current) > session_candidate_activity_mtime(&stale)
        );

        let found = find_kimi_session_in(&kimi_home, SESSION).unwrap();
        assert_eq!(found.work_dir.as_deref(), Some("/current"));
    }

    #[test]
    fn directory_walk_has_an_entry_budget_even_without_session_hits() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..10 {
            std::fs::create_dir_all(dir.path().join(format!("not-a-session-{index}"))).unwrap();
        }
        let mut found = Vec::new();
        let mut budget = 3;
        collect_session_dirs(dir.path(), 2, &mut found, 100, &mut budget);
        assert_eq!(budget, 0);
        assert!(found.is_empty());
    }
}
