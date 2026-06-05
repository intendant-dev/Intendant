use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct FissionLedger {
    pub groups: Vec<FissionGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FissionGroup {
    pub group_id: String,
    pub parent_session_id: String,
    pub anchor_item_id: String,
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_session_id: Option<String>,
    pub branches: Vec<FissionBranch>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FissionBranch {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_session_id: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    pub raw_log: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ephemeral: bool,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FissionObservation {
    pub parent_session_id: String,
    pub anchor_item_id: String,
    pub tool: String,
    pub status: String,
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub branches: Vec<FissionBranchObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FissionBranchObservation {
    pub session_id: String,
    pub status: String,
    pub summary: Option<String>,
}

#[derive(Debug)]
pub enum ClaimCanonicalError {
    Io(io::Error),
    GroupNotFound(String),
    BranchNotFound {
        group_id: String,
        branch_session_id: String,
    },
    Conflict {
        group_id: String,
        expected: Option<String>,
        current: Option<String>,
    },
}

impl fmt::Display for ClaimCanonicalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::GroupNotFound(group_id) => {
                write!(f, "fission group `{group_id}` was not found")
            }
            Self::BranchNotFound {
                group_id,
                branch_session_id,
            } => write!(
                f,
                "branch `{branch_session_id}` is not part of fission group `{group_id}`"
            ),
            Self::Conflict {
                group_id,
                expected,
                current,
            } => write!(
                f,
                "canonical claim conflict for `{group_id}`: expected {}, current {}",
                display_optional_id(expected),
                display_optional_id(current)
            ),
        }
    }
}

impl std::error::Error for ClaimCanonicalError {}

impl From<io::Error> for ClaimCanonicalError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn ledger_path(log_dir: &Path) -> PathBuf {
    log_dir.join("fission_ledger.json")
}

pub fn group_id(parent_session_id: &str, anchor_item_id: &str) -> String {
    // The slugs are lossy (non-alphanumerics collapse to `_`, truncated to 96
    // chars) and are joined with `-`, which is itself a legal slug char — so
    // distinct (parent, anchor) pairs can slug to the same string. Append a
    // stable hash of the exact raw bytes so the id stays collision-resistant
    // while the slug remains human-readable.
    format!(
        "fission-{}-{}-{}",
        stable_slug(parent_session_id),
        stable_slug(anchor_item_id),
        stable_pair_hash(parent_session_id, anchor_item_id),
    )
}

pub fn read_fission_ledger(log_dir: &Path) -> io::Result<Option<FissionLedger>> {
    let path = ledger_path(log_dir);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(io::Error::other)
}

pub fn read_fission_ledger_for_session(
    log_dir: &Path,
    session_id: &str,
) -> io::Result<Option<FissionLedger>> {
    let Some(ledger) = read_fission_ledger(log_dir)? else {
        return Ok(None);
    };
    Ok(filter_ledger_for_session(ledger, session_id))
}

pub fn persist_fission_ledger(log_dir: &Path, ledger: &FissionLedger) -> io::Result<()> {
    fs::create_dir_all(log_dir)?;
    let bytes = serde_json::to_vec_pretty(ledger).map_err(io::Error::other)?;
    // Atomic write so a crash mid-write can't truncate the ledger into invalid
    // JSON that the read side would then silently drop.
    crate::file_watcher::atomic_write(&ledger_path(log_dir), &bytes)
}

pub fn record_fission_observation(
    log_dir: &Path,
    observation: FissionObservation,
) -> io::Result<Option<FissionGroup>> {
    let _guard = ledger_write_lock();
    let parent_session_id = clean_string(&observation.parent_session_id);
    let anchor_item_id = clean_string(&observation.anchor_item_id);
    if parent_session_id.is_none() || anchor_item_id.is_none() {
        return Ok(None);
    }
    let parent_session_id = parent_session_id.unwrap();
    let anchor_item_id = anchor_item_id.unwrap();
    let tool = clean_string(&observation.tool).unwrap_or_else(|| "spawn_agent".to_string());
    let now = chrono::Utc::now().to_rfc3339();
    let group_id = group_id(&parent_session_id, &anchor_item_id);
    let mut ledger = read_fission_ledger(log_dir)?.unwrap_or_default();
    let idx = ledger
        .groups
        .iter()
        .position(|group| group.group_id == group_id);
    let group = if let Some(idx) = idx {
        &mut ledger.groups[idx]
    } else {
        ledger.groups.push(FissionGroup {
            group_id: group_id.clone(),
            parent_session_id: parent_session_id.clone(),
            anchor_item_id: anchor_item_id.clone(),
            tool: tool.clone(),
            objective: clean_string(observation.prompt.as_deref().unwrap_or_default()),
            prompt: clean_string(observation.prompt.as_deref().unwrap_or_default()),
            created_at: now.clone(),
            updated_at: now.clone(),
            canonical_session_id: None,
            branches: Vec::new(),
        });
        ledger.groups.last_mut().expect("pushed group")
    };

    group.parent_session_id = parent_session_id;
    group.anchor_item_id = anchor_item_id;
    group.tool = tool;
    if let Some(prompt) = clean_string(observation.prompt.as_deref().unwrap_or_default()) {
        group.objective = Some(prompt.clone());
        group.prompt = Some(prompt);
    }
    group.updated_at = now.clone();

    for branch in observation.branches {
        let Some(session_id) = clean_string(&branch.session_id) else {
            continue;
        };
        if session_id == group.parent_session_id {
            continue;
        }
        let status = normalize_status(
            clean_string(&branch.status)
                .as_deref()
                .unwrap_or(&observation.status),
        );
        let summary = clean_string(branch.summary.as_deref().unwrap_or_default());
        let raw_log = format!("session.jsonl#session_id={session_id}");
        let branch_idx = group
            .branches
            .iter()
            .position(|existing| existing.session_id == session_id);
        if let Some(idx) = branch_idx {
            let existing = &mut group.branches[idx];
            // Don't let a stale/coarser observation downgrade a terminal status
            // (e.g. a receiver-only `wait`/`completed` collab call re-recording an
            // already-completed child as `running`).
            if !is_terminal_status(&existing.status) || is_terminal_status(&status) {
                existing.status = status;
            }
            if summary.is_some() {
                existing.summary = summary;
            }
            if existing.task.is_none() {
                existing.task = group.objective.clone();
            }
            if existing.model.is_none() {
                existing.model = clean_string(observation.model.as_deref().unwrap_or_default());
            }
            if existing.reasoning_effort.is_none() {
                existing.reasoning_effort =
                    clean_string(observation.reasoning_effort.as_deref().unwrap_or_default());
            }
            existing.updated_at = now.clone();
        } else {
            group.branches.push(FissionBranch {
                backend_session_id: Some(session_id.clone()),
                status,
                summary,
                task: group.objective.clone(),
                model: clean_string(observation.model.as_deref().unwrap_or_default()),
                reasoning_effort: clean_string(
                    observation.reasoning_effort.as_deref().unwrap_or_default(),
                ),
                worktree_path: None,
                raw_log,
                ephemeral: false,
                updated_at: now.clone(),
                session_id,
            });
        }
    }
    group
        .branches
        .sort_by(|a, b| a.session_id.cmp(&b.session_id));
    let updated = group.clone();
    persist_fission_ledger(log_dir, &ledger)?;
    Ok(Some(updated))
}

pub fn claim_canonical(
    log_dir: &Path,
    group_id: &str,
    branch_session_id: &str,
    expected_canonical_session_id: Option<&str>,
) -> Result<FissionGroup, ClaimCanonicalError> {
    let _guard = ledger_write_lock();
    let group_id = clean_string(group_id).unwrap_or_default();
    let branch_session_id = clean_string(branch_session_id).unwrap_or_default();
    let expected = expected_canonical_session_id.and_then(clean_string);
    let mut ledger = read_fission_ledger(log_dir)?.unwrap_or_default();
    let group = ledger
        .groups
        .iter_mut()
        .find(|group| group.group_id == group_id)
        .ok_or_else(|| ClaimCanonicalError::GroupNotFound(group_id.clone()))?;
    if !group
        .branches
        .iter()
        .any(|branch| branch.session_id == branch_session_id)
    {
        return Err(ClaimCanonicalError::BranchNotFound {
            group_id,
            branch_session_id,
        });
    }

    let current = group.canonical_session_id.clone();
    if let Some(expected) = expected {
        if current.as_deref() != Some(expected.as_str()) {
            return Err(ClaimCanonicalError::Conflict {
                group_id,
                expected: Some(expected),
                current,
            });
        }
    } else if current
        .as_deref()
        .is_some_and(|current| current != branch_session_id)
    {
        return Err(ClaimCanonicalError::Conflict {
            group_id,
            expected: None,
            current,
        });
    }

    group.canonical_session_id = Some(branch_session_id);
    group.updated_at = chrono::Utc::now().to_rfc3339();
    let updated = group.clone();
    persist_fission_ledger(log_dir, &ledger)?;
    Ok(updated)
}

fn filter_ledger_for_session(ledger: FissionLedger, session_id: &str) -> Option<FissionLedger> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return if ledger.groups.is_empty() {
            None
        } else {
            Some(ledger)
        };
    }

    let mut related: BTreeSet<String> = [session_id.to_string()].into_iter().collect();
    loop {
        let before = related.len();
        for group in &ledger.groups {
            let group_touches_related = related.contains(&group.parent_session_id)
                || group
                    .canonical_session_id
                    .as_ref()
                    .is_some_and(|id| related.contains(id))
                || group
                    .branches
                    .iter()
                    .any(|branch| related.contains(&branch.session_id));
            if group_touches_related {
                related.insert(group.parent_session_id.clone());
                if let Some(canonical) = &group.canonical_session_id {
                    related.insert(canonical.clone());
                }
                for branch in &group.branches {
                    related.insert(branch.session_id.clone());
                }
            }
        }
        if related.len() == before {
            break;
        }
    }

    let groups: Vec<FissionGroup> = ledger
        .groups
        .into_iter()
        .filter(|group| {
            related.contains(&group.parent_session_id)
                || group
                    .canonical_session_id
                    .as_ref()
                    .is_some_and(|id| related.contains(id))
                || group
                    .branches
                    .iter()
                    .any(|branch| related.contains(&branch.session_id))
        })
        .collect();
    if groups.is_empty() {
        None
    } else {
        Some(FissionLedger { groups })
    }
}

fn normalize_status(status: &str) -> String {
    match status.trim() {
        "inProgress" | "pendingInit" => "running".to_string(),
        "errored" => "failed".to_string(),
        // `notFound` is frequently transient (a state lookup miss while a child is
        // starting/migrating); treat it as non-terminal `unknown` rather than
        // conflating it with a definitive failure.
        "notFound" => "unknown".to_string(),
        "shutdown" => "ended".to_string(),
        "completed" | "interrupted" | "failed" | "running" => status.trim().to_string(),
        other if !other.is_empty() => other.to_string(),
        _ => "running".to_string(),
    }
}

fn clean_string(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// FNV-1a 64-bit fold; deterministic across processes and crate versions
/// (unlike `std`'s `DefaultHasher`), so it is safe for a persisted, equality-
/// matched key.
fn fnv1a(mut hash: u64, bytes: &[u8]) -> u64 {
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Stable hex hash of the exact (parent, anchor) bytes, length-prefixed so a
/// byte that straddles the field boundary can't forge a collision.
fn stable_pair_hash(parent_session_id: &str, anchor_item_id: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    hash = fnv1a(hash, &(parent_session_id.len() as u64).to_le_bytes());
    hash = fnv1a(hash, parent_session_id.as_bytes());
    hash = fnv1a(hash, &(anchor_item_id.len() as u64).to_le_bytes());
    hash = fnv1a(hash, anchor_item_id.as_bytes());
    format!("{hash:016x}")
}

/// Terminal branch statuses that a later, coarser observation must not downgrade.
fn is_terminal_status(status: &str) -> bool {
    matches!(
        status.trim(),
        "completed" | "failed" | "ended" | "interrupted" | "cancelled"
    )
}

fn stable_slug(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let out = out.trim_matches('_');
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out.chars().take(96).collect()
    }
}

fn display_optional_id(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("<none>")
}

fn ledger_write_lock() -> MutexGuard<'static, ()> {
    static LEDGER_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LEDGER_WRITE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn observation(status: &str) -> FissionObservation {
        FissionObservation {
            parent_session_id: "parent".to_string(),
            anchor_item_id: "call-123".to_string(),
            tool: "spawn_agent".to_string(),
            status: status.to_string(),
            prompt: Some("inspect parser".to_string()),
            model: Some("gpt-5.2-codex".to_string()),
            reasoning_effort: Some("high".to_string()),
            branches: vec![FissionBranchObservation {
                session_id: "child".to_string(),
                status: status.to_string(),
                summary: None,
            }],
        }
    }

    #[test]
    fn records_fission_group_by_exact_spawn_anchor() {
        let dir = tempdir().unwrap();
        let group = record_fission_observation(dir.path(), observation("inProgress"))
            .unwrap()
            .expect("group");

        assert_eq!(group.group_id, group_id("parent", "call-123"));
        assert_eq!(group.parent_session_id, "parent");
        assert_eq!(group.anchor_item_id, "call-123");
        assert_eq!(group.objective.as_deref(), Some("inspect parser"));
        assert_eq!(group.branches.len(), 1);
        assert_eq!(group.branches[0].session_id, "child");
        assert_eq!(group.branches[0].status, "running");

        let ledger = read_fission_ledger(dir.path()).unwrap().expect("ledger");
        assert_eq!(ledger.groups, vec![group]);
    }

    #[test]
    fn updates_existing_branch_status_and_summary() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("inProgress")).unwrap();
        let mut done = observation("completed");
        done.branches[0].summary = Some("parser is fine".to_string());
        let group = record_fission_observation(dir.path(), done)
            .unwrap()
            .expect("group");

        assert_eq!(group.branches.len(), 1);
        assert_eq!(group.branches[0].status, "completed");
        assert_eq!(group.branches[0].summary.as_deref(), Some("parser is fine"));
    }

    #[test]
    fn terminal_status_is_not_downgraded_by_later_running_observation() {
        let dir = tempdir().unwrap();
        let mut done = observation("completed");
        done.branches[0].summary = Some("done".to_string());
        record_fission_observation(dir.path(), done).unwrap();
        // A later coarser observation reports the child as running again.
        let group = record_fission_observation(dir.path(), observation("running"))
            .unwrap()
            .expect("group");
        assert_eq!(group.branches[0].status, "completed");
        assert_eq!(group.branches[0].summary.as_deref(), Some("done"));
    }

    #[test]
    fn group_id_is_collision_resistant_across_separator_ambiguity() {
        // (x, y-z) and (x-y, z) slug to the same "fission-x-y-z" prefix; the hash
        // suffix must keep the two group ids distinct.
        assert_ne!(group_id("x", "y-z"), group_id("x-y", "z"));
    }

    #[test]
    fn canonical_claim_is_first_writer_wins_without_expected_id() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let gid = group_id("parent", "call-123");
        let claimed = claim_canonical(dir.path(), &gid, "child", None).unwrap();
        assert_eq!(claimed.canonical_session_id.as_deref(), Some("child"));

        let err = claim_canonical(dir.path(), &gid, "other", None).unwrap_err();
        assert!(matches!(err, ClaimCanonicalError::BranchNotFound { .. }));
    }

    #[test]
    fn canonical_claim_honors_compare_and_swap() {
        let dir = tempdir().unwrap();
        let mut obs = observation("running");
        obs.branches.push(FissionBranchObservation {
            session_id: "child-2".to_string(),
            status: "running".to_string(),
            summary: None,
        });
        record_fission_observation(dir.path(), obs).unwrap();
        let gid = group_id("parent", "call-123");
        claim_canonical(dir.path(), &gid, "child", None).unwrap();

        let err = claim_canonical(dir.path(), &gid, "child-2", Some("child-2")).unwrap_err();
        assert!(matches!(err, ClaimCanonicalError::Conflict { .. }));

        let claimed = claim_canonical(dir.path(), &gid, "child-2", Some("child")).unwrap();
        assert_eq!(claimed.canonical_session_id.as_deref(), Some("child-2"));
    }

    #[test]
    fn filters_ledger_to_related_session_component() {
        let dir = tempdir().unwrap();
        record_fission_observation(dir.path(), observation("running")).unwrap();
        let unrelated = FissionObservation {
            parent_session_id: "other-parent".to_string(),
            anchor_item_id: "other-call".to_string(),
            tool: "spawn_agent".to_string(),
            status: "running".to_string(),
            prompt: None,
            model: None,
            reasoning_effort: None,
            branches: vec![FissionBranchObservation {
                session_id: "other-child".to_string(),
                status: "running".to_string(),
                summary: None,
            }],
        };
        record_fission_observation(dir.path(), unrelated).unwrap();

        let ledger = read_fission_ledger_for_session(dir.path(), "child")
            .unwrap()
            .expect("ledger");
        assert_eq!(ledger.groups.len(), 1);
        assert_eq!(ledger.groups[0].parent_session_id, "parent");
    }
}
