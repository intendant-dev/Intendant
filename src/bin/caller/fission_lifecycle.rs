//! Fission branch lifecycle: the runtime contract between the fission MCP
//! surface (`mcp.rs`) and the supervisor core (`main.rs`).
//!
//! This module owns the in-process registry mapping spawned branch sessions to
//! their fission group + registering log dir, the ledger-backed wait helper
//! used by `fission_control(op="wait")`, and (once wired) the bus watcher that
//! feeds branch lifecycle events into the durable fission ledger.
//!
//! The function signatures here are a frozen contract: the MCP stage and the
//! supervisor stage compile against them independently. Implementation TODOs
//! are marked for the supervisor stage.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::fission_ledger::{self, FissionGroup};

/// Where a spawned fission branch reports: the group it belongs to and the
/// log dir whose `fission_ledger.json` carries the group. Registered by the
/// spawn handler; consumed by the lifecycle watcher and the wait helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRoute {
    pub group_id: String,
    pub log_dir: PathBuf,
}

fn registry() -> &'static Mutex<HashMap<String, BranchRoute>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, BranchRoute>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a freshly spawned fission branch. Called by the supervisor's
/// `fission_spawn` handler right after `register_spawned_branch` persists the
/// ledger entry.
pub fn register_branch(branch_session_id: &str, group_id: &str, log_dir: &Path) {
    let branch_session_id = branch_session_id.trim();
    if branch_session_id.is_empty() {
        return;
    }
    registry().lock().unwrap().insert(
        branch_session_id.to_string(),
        BranchRoute {
            group_id: group_id.to_string(),
            log_dir: log_dir.to_path_buf(),
        },
    );
}

/// Look up the route for a spawned branch, if it was registered in this
/// process (or rehydrated at startup).
pub fn branch_route(branch_session_id: &str) -> Option<BranchRoute> {
    registry()
        .lock()
        .unwrap()
        .get(branch_session_id.trim())
        .cloned()
}

/// Drop any parent-facing delivery routing for the given fission groups.
/// Called by the rewind path immediately after `detach_groups_with_invalid_anchors`
/// so a detached branch's later completion cannot auto-deliver into the
/// rewound parent.
pub fn drop_pending_deliveries(group_ids: &[String]) {
    if group_ids.is_empty() {
        return;
    }
    registry()
        .lock()
        .unwrap()
        .retain(|_, route| !group_ids.contains(&route.group_id));
}

/// Rehydrate the in-process registry from persisted fission ledgers under
/// `~/.intendant/logs/*/fission_ledger.json`, registering routes for branches
/// that are not yet terminal.
///
/// TODO(supervisor stage): scan ledger documents, skip detached groups, and
/// return the number of rehydrated routes.
pub fn rehydrate_from_logs(_logs_dir: &Path) -> io::Result<usize> {
    Ok(0)
}

/// Outcome of waiting on a fission branch (or any branch of a group).
#[derive(Debug, Clone)]
pub enum WaitOutcome {
    /// The watched branch reached a terminal status; snapshot of the group.
    Terminal(FissionGroup),
    /// Timeout elapsed while the branch was still running. This is a normal
    /// result, not an error — callers report `still_running` and continue.
    StillRunning(FissionGroup),
    /// The group was detached by a context rewind; waiting is refused.
    Detached(FissionGroup),
    GroupNotFound,
    BranchNotFound(FissionGroup),
}

/// Block (async) until the watched branch reaches a terminal status, the
/// group detaches, or the timeout elapses. `branch_session_id = None` waits
/// for ANY branch of the group to become terminal.
///
/// Ledger-poll implementation (1s cadence). The supervisor stage may layer
/// bus-event wakeups on top; the polling contract and return shape stay.
pub async fn wait_for_branch_terminal(
    log_dir: &Path,
    group_id: &str,
    branch_session_id: Option<&str>,
    timeout: Duration,
) -> io::Result<WaitOutcome> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let snapshot = read_group(log_dir, group_id)?;
        let Some((group, detached)) = snapshot else {
            return Ok(WaitOutcome::GroupNotFound);
        };
        if detached {
            return Ok(WaitOutcome::Detached(group));
        }
        match branch_session_id.map(str::trim).filter(|id| !id.is_empty()) {
            Some(branch_id) => {
                let Some(branch) = group
                    .branches
                    .iter()
                    .find(|branch| branch.session_id == branch_id)
                else {
                    return Ok(WaitOutcome::BranchNotFound(group));
                };
                if fission_ledger::branch_status_is_terminal(&branch.status) {
                    return Ok(WaitOutcome::Terminal(group));
                }
            }
            None => {
                if group
                    .branches
                    .iter()
                    .any(|branch| fission_ledger::branch_status_is_terminal(&branch.status))
                {
                    return Ok(WaitOutcome::Terminal(group));
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(WaitOutcome::StillRunning(group));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn read_group(log_dir: &Path, group_id: &str) -> io::Result<Option<(FissionGroup, bool)>> {
    let Some(document) = fission_ledger::read_fission_ledger_document(log_dir)? else {
        return Ok(None);
    };
    let detached = document.group_is_detached(group_id);
    Ok(document
        .into_ledger()
        .groups
        .into_iter()
        .find(|group| group.group_id == group_id)
        .map(|group| (group, detached)))
}

/// Spawn the bus watcher that feeds branch session lifecycle events
/// (DoneSignal/TaskComplete/SessionEnded/Interrupted, FileChanged diffs) into
/// the fission ledger for registered branches.
///
/// TODO(supervisor stage): implement the event mapping (terminal statuses via
/// the registry routes, summaries from done messages, changed-files
/// accumulation) honoring the ledger's sticky/terminal status rules.
pub fn spawn_fission_lifecycle_watcher(
    mut rx: tokio::sync::broadcast::Receiver<crate::event::AppEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(_event) => {
                    // TODO(supervisor stage): map lifecycle events into
                    // fission_ledger status/work updates for registered routes.
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fission_ledger::{BranchCharter, NewSpawnedBranch};
    use tempfile::tempdir;

    fn register_test_branch(log_dir: &Path, parent: &str, anchor: &str, session: &str) -> String {
        let group = fission_ledger::register_spawned_branch(
            log_dir,
            parent,
            anchor,
            BranchCharter {
                objective: "test objective".to_string(),
                write_scope: None,
                worktree_requested: false,
            },
            NewSpawnedBranch {
                session_id: session.to_string(),
                backend_session_id: Some(session.to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        group.group_id
    }

    #[test]
    fn registry_round_trip_and_group_drop() {
        let dir = tempdir().unwrap();
        register_branch("branch-1", "group-a", dir.path());
        register_branch("branch-2", "group-b", dir.path());
        assert_eq!(
            branch_route("branch-1").unwrap().group_id,
            "group-a".to_string()
        );
        drop_pending_deliveries(&["group-a".to_string()]);
        assert!(branch_route("branch-1").is_none());
        assert!(branch_route("branch-2").is_some());
        drop_pending_deliveries(&["group-b".to_string()]);
    }

    #[tokio::test]
    async fn wait_reports_still_running_then_terminal() {
        let dir = tempdir().unwrap();
        let group_id = register_test_branch(dir.path(), "parent-1", "call-1", "child-1");

        let outcome = wait_for_branch_terminal(
            dir.path(),
            &group_id,
            Some("child-1"),
            Duration::from_millis(10),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, WaitOutcome::StillRunning(_)));

        fission_ledger::record_fission_observation(
            dir.path(),
            fission_ledger::FissionObservation {
                parent_session_id: "parent-1".to_string(),
                anchor_item_id: "call-1".to_string(),
                tool: "fission_spawn".to_string(),
                status: "completed".to_string(),
                prompt: None,
                model: None,
                reasoning_effort: None,
                branches: vec![fission_ledger::FissionBranchObservation {
                    session_id: "child-1".to_string(),
                    status: "completed".to_string(),
                    summary: Some("done".to_string()),
                }],
            },
        )
        .unwrap();

        let outcome = wait_for_branch_terminal(
            dir.path(),
            &group_id,
            Some("child-1"),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(matches!(outcome, WaitOutcome::Terminal(_)));
    }

    #[tokio::test]
    async fn wait_refuses_detached_groups_and_reports_missing() {
        let dir = tempdir().unwrap();
        let group_id = register_test_branch(dir.path(), "parent-2", "call-9", "child-9");
        fission_ledger::detach_group(dir.path(), &group_id, "rewind crossed anchor").unwrap();

        let outcome =
            wait_for_branch_terminal(dir.path(), &group_id, None, Duration::from_millis(10))
                .await
                .unwrap();
        assert!(matches!(outcome, WaitOutcome::Detached(_)));

        let outcome =
            wait_for_branch_terminal(dir.path(), "missing-group", None, Duration::from_millis(10))
                .await
                .unwrap();
        assert!(matches!(outcome, WaitOutcome::GroupNotFound));
    }
}
