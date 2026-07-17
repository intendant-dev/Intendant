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

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::event::AppEvent;
use crate::fission_ledger::{self, FissionGroup};

/// Boilerplate the session log writes for a `done_signal` with no caller
/// message (see `SessionLog::done_signal_for_session`). Filtered out — same
/// convention as the lineage ledger — so it isn't recorded as a
/// model-authored branch summary.
const DONE_SIGNAL_DEFAULT_MESSAGE: &str = "Agent signalled done";

/// Cap on branch-summary length persisted by the watcher; mirrors the lineage
/// ledger's `trim_summary` convention.
const BRANCH_SUMMARY_MAX_CHARS: usize = 240;

/// Upper bound on changed-file entries the watcher accumulates per branch, so
/// a long-running branch in a churny checkout cannot grow the ledger without
/// bound.
const CHANGED_FILES_PER_BRANCH_CAP: usize = 200;

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
/// that are not yet terminal. Detached groups are skipped: their pending
/// deliveries were already dropped when the rewind severed the anchor, and a
/// detached branch must not regain a parent-facing route across a restart.
///
/// Returns the number of registered routes. Unreadable or corrupt ledger
/// files are skipped rather than failing the whole rehydration — a single
/// bad session directory must not block daemon startup.
pub fn rehydrate_from_logs(logs_dir: &Path) -> io::Result<usize> {
    let entries = match fs::read_dir(logs_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    let mut rehydrated = 0usize;
    for entry in entries.flatten() {
        let log_dir = entry.path();
        if !log_dir.is_dir() {
            continue;
        }
        let Ok(Some(document)) = fission_ledger::read_fission_ledger_document(&log_dir) else {
            continue;
        };
        for group in &document.groups {
            if document.group_is_detached(&group.group_id) {
                continue;
            }
            for branch in &group.branches {
                if fission_ledger::branch_status_is_terminal(&branch.status) {
                    continue;
                }
                register_branch(&branch.session_id, &group.group_id, &log_dir);
                rehydrated += 1;
            }
        }
    }
    Ok(rehydrated)
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
/// Ledger-poll implementation (1s cadence), fingerprint-guarded: the ~MB
/// ledger is only re-read and re-parsed when its (len, mtime) moved — every
/// write goes through an atomic rename, so unchanged metadata means an
/// unchanged ledger and the previous evaluation still holds. The supervisor
/// stage may layer bus-event wakeups on top; the polling contract and
/// return shape stay.
pub async fn wait_for_branch_terminal(
    log_dir: &Path,
    group_id: &str,
    branch_session_id: Option<&str>,
    timeout: Duration,
) -> io::Result<WaitOutcome> {
    let deadline = tokio::time::Instant::now() + timeout;
    let ledger_path = fission_ledger::ledger_path(log_dir);
    let mut last_fingerprint: Option<Option<(u64, u128)>> = None;
    let mut snapshot: Option<(FissionGroup, bool)> = None;
    loop {
        let fingerprint = ledger_file_fingerprint(&ledger_path);
        if last_fingerprint != Some(fingerprint) {
            snapshot = read_group(log_dir, group_id)?;
            last_fingerprint = Some(fingerprint);
        }
        let Some((group, detached)) = snapshot.as_ref() else {
            return Ok(WaitOutcome::GroupNotFound);
        };
        if *detached {
            return Ok(WaitOutcome::Detached(group.clone()));
        }
        match branch_session_id.map(str::trim).filter(|id| !id.is_empty()) {
            Some(branch_id) => {
                let Some(branch) = group
                    .branches
                    .iter()
                    .find(|branch| branch.session_id == branch_id)
                else {
                    return Ok(WaitOutcome::BranchNotFound(group.clone()));
                };
                if fission_ledger::branch_status_is_terminal(&branch.status) {
                    return Ok(WaitOutcome::Terminal(group.clone()));
                }
            }
            None => {
                if group
                    .branches
                    .iter()
                    .any(|branch| fission_ledger::branch_status_is_terminal(&branch.status))
                {
                    return Ok(WaitOutcome::Terminal(group.clone()));
                }
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(WaitOutcome::StillRunning(group.clone()));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// (len, mtime-nanos) of the ledger file; `None` when it does not exist.
/// Same change-detection discipline as the wrapper-index cache.
fn ledger_file_fingerprint(path: &Path) -> Option<(u64, u128)> {
    let metadata = fs::metadata(path).ok()?;
    let mtime_nanos = metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    Some((metadata.len(), mtime_nanos))
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
/// Status mapping (mirrors the lineage ledger's interpretation of the same
/// bus events):
/// - `DoneSignal` / `TaskComplete` → `completed`, summary from the done
///   message (the writer's "Agent signalled done" boilerplate is filtered);
/// - `Interrupted` → the sticky `cancelled`;
/// - `SessionEnded` without a prior terminal status → `failed` for
///   error-shaped teardown reasons (`"error: …"`, "… errored …"), otherwise
///   `ended` (a generic teardown never claims `completed` directly; `ended`
///   normalizes to it). A branch already terminal keeps its status — mirror
///   of the lineage rule that a teardown must not downgrade a completed task.
///
/// All writes go through [`fission_ledger::record_fission_observation`] /
/// [`fission_ledger::update_branch_work`], so the ledger's sticky/terminal
/// no-downgrade rules apply unchanged.
///
/// The watcher consumes the bus's lossless session-log lane, not the bounded
/// broadcast ring: the ledger is durable state, and a terminal event dropped
/// under `RecvError::Lagged` (a delta flood is enough to evict one) left the
/// branch marked `running` forever and rehydrated it as such after restart.
/// Every event this watcher folds — `DoneSignal`, `TaskComplete`,
/// `Interrupted`, `SessionEnded`, `FileChanged` — is in the
/// session-log subset, and the lane also spares the watcher the per-token
/// `ModelResponseDelta` traffic the broadcast carries.
pub fn spawn_fission_lifecycle_watcher(
    bus: &crate::event::EventBus,
) -> tokio::task::JoinHandle<()> {
    let mut rx = bus.subscribe_session_log();
    tokio::spawn(async move {
        let mut state = LifecycleWatcherState::default();
        // The lane pairs each event with its enqueue instant (the session-log
        // writer's latency forensics); this watcher only folds the events.
        while let Some((event, _enqueued_at)) = rx.recv().await {
            if !matches!(event, AppEvent::FileChanged { .. }) {
                handle_lifecycle_event(&event, &mut state);
                continue;
            }
            // Changed-file burst: drain every immediately available event,
            // staging FileChanged paths (dedup/cap applied per event, in
            // stream order) and flushing the whole batch as ONE ledger
            // read→modify→write per log dir — a save storm otherwise costs
            // one fsync'd multi-MB rewrite per file per branch. Non-file
            // events flush what preceded them first, preserving the
            // pre-terminal/post-terminal accumulation order.
            let mut batch = ChangedFileBatch::default();
            stage_lifecycle_event(&event, &mut state, &mut batch);
            loop {
                match rx.try_recv() {
                    Ok((next @ AppEvent::FileChanged { .. }, _)) => {
                        stage_lifecycle_event(&next, &mut state, &mut batch);
                    }
                    Ok((next, _)) => {
                        batch.flush();
                        handle_lifecycle_event(&next, &mut state);
                    }
                    Err(_) => break,
                }
            }
            batch.flush();
        }
    })
}

/// Watcher-local accumulation state. Lives inside the watcher task; the
/// durable facts are in the fission ledger — this only suppresses redundant
/// ledger writes (changed-file repeats) and post-terminal accumulation.
#[derive(Default)]
struct LifecycleWatcherState {
    /// Branch sessions the watcher has seen reach a terminal status; stops
    /// changed-file accumulation (their work is done — later project churn
    /// belongs to whoever is still running).
    terminal_branches: HashSet<String>,
    /// Per-branch changed-file paths already persisted: local dedup plus the
    /// [`CHANGED_FILES_PER_BRANCH_CAP`] bound, so a noisy file watcher does
    /// not rewrite `fission_ledger.json` for every repeated save.
    recorded_changed_files: HashMap<String, HashSet<String>>,
}

/// Synchronous event-mapping core of the lifecycle watcher; factored out of
/// the spawn loop so tests can drive it deterministically.
fn handle_lifecycle_event(event: &AppEvent, state: &mut LifecycleWatcherState) {
    match event {
        AppEvent::DoneSignal {
            session_id,
            message,
        } => {
            let summary = message
                .as_deref()
                .map(str::trim)
                .filter(|message| !message.is_empty() && *message != DONE_SIGNAL_DEFAULT_MESSAGE)
                .map(trim_branch_summary);
            record_terminal_status(state, session_id.as_deref(), "completed", summary);
        }
        AppEvent::TaskComplete {
            session_id,
            reason,
            summary,
        } => {
            let summary = summary
                .as_deref()
                .or(Some(reason.as_str()))
                .map(str::trim)
                .filter(|summary| !summary.is_empty())
                .map(trim_branch_summary);
            record_terminal_status(state, session_id.as_deref(), "completed", summary);
        }
        AppEvent::Interrupted { session_id, .. } => {
            record_terminal_status(state, session_id.as_deref(), "cancelled", None);
        }
        AppEvent::SessionEnded {
            session_id, reason, ..
        } => {
            record_session_ended(state, session_id, reason);
        }
        AppEvent::FileChanged { path, .. } => {
            record_changed_file(state, path);
        }
        _ => {}
    }
}

/// Record a terminal lifecycle status for a registered branch. Routes through
/// [`fission_ledger::record_fission_observation`], which already refuses to
/// overwrite sticky statuses or downgrade terminal ones.
fn record_terminal_status(
    state: &mut LifecycleWatcherState,
    session_id: Option<&str>,
    status: &str,
    summary: Option<String>,
) {
    let Some(session_id) = session_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return;
    };
    let Some(route) = branch_route(session_id) else {
        return;
    };
    if record_branch_observation(&route, session_id, status, summary) {
        state.terminal_branches.insert(session_id.to_string());
    }
}

/// `SessionEnded` mapping: a generic teardown must not downgrade a branch
/// that already reached a terminal status (mirrors the lineage ledger's
/// `session_ended` rule), records `failed` for error-shaped reasons and
/// `ended` otherwise, and only adopts the teardown reason as a summary when
/// the branch has none yet.
fn record_session_ended(state: &mut LifecycleWatcherState, session_id: &str, reason: &str) {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return;
    }
    let Some(route) = branch_route(session_id) else {
        return;
    };
    let Ok(Some((group, _detached))) = read_group(&route.log_dir, &route.group_id) else {
        return;
    };
    let Some(branch) = group
        .branches
        .iter()
        .find(|branch| branch.session_id == session_id)
    else {
        return;
    };
    if fission_ledger::branch_status_is_terminal(&branch.status) {
        state.terminal_branches.insert(session_id.to_string());
        return;
    }
    let status = if session_ended_reason_is_failure(reason) {
        "failed"
    } else {
        "ended"
    };
    let summary = if branch.summary.is_none() {
        let reason = reason.trim();
        (!reason.is_empty()).then(|| trim_branch_summary(reason))
    } else {
        None
    };
    if record_branch_observation(&route, session_id, status, summary) {
        state.terminal_branches.insert(session_id.to_string());
    }
}

/// True for `SessionEnded` reasons that describe a failure rather than a
/// normal teardown. Matches the reason shapes the emitters produce: the agent
/// loop's `"error: {e}"` summary and the Codex subagent terminal
/// `"Codex subagent errored…"`.
fn session_ended_reason_is_failure(reason: &str) -> bool {
    let reason = reason.trim().to_ascii_lowercase();
    reason.starts_with("error") || reason.contains("errored") || reason.contains("failed")
}

/// Feed one lifecycle status observation for a registered branch into its
/// group's ledger. Reads the group to address the observation at the exact
/// `(parent, anchor)` the spawn registered; a missing ledger or group means a
/// stale route and is ignored.
fn record_branch_observation(
    route: &BranchRoute,
    branch_session_id: &str,
    status: &str,
    summary: Option<String>,
) -> bool {
    let Ok(Some((group, _detached))) = read_group(&route.log_dir, &route.group_id) else {
        return true;
    };
    match fission_ledger::record_fission_observation(
        &route.log_dir,
        fission_ledger::FissionObservation {
            parent_session_id: group.parent_session_id.clone(),
            anchor_item_id: group.anchor_item_id.clone(),
            tool: group.tool.clone(),
            status: status.to_string(),
            prompt: None,
            model: None,
            reasoning_effort: None,
            branches: vec![fission_ledger::FissionBranchObservation {
                session_id: branch_session_id.to_string(),
                status: status.to_string(),
                summary,
            }],
        },
    ) {
        Ok(_) => true,
        Err(err) => {
            eprintln!(
                "[fission-lifecycle] failed to record terminal observation for branch {}: {}",
                branch_session_id, err
            );
            false
        }
    }
}

/// Changed-file deltas staged per (log dir, group, branch), flushed as one
/// bulk ledger write per log dir ([`fission_ledger::update_branches_changed_files`]).
#[derive(Default)]
struct ChangedFileBatch {
    staged: HashMap<PathBuf, HashMap<(String, String), Vec<String>>>,
}

impl ChangedFileBatch {
    fn flush(&mut self) {
        for (log_dir, branches) in self.staged.drain() {
            let updates: Vec<fission_ledger::BranchChangedFiles> = branches
                .into_iter()
                .map(|((group_id, branch_session_id), changed_files)| {
                    fission_ledger::BranchChangedFiles {
                        group_id,
                        branch_session_id,
                        changed_files,
                    }
                })
                .collect();
            if let Err(err) = fission_ledger::update_branches_changed_files(&log_dir, &updates) {
                eprintln!("[fission-lifecycle] failed to record changed files: {err}");
            }
        }
    }
}

/// Stage `FileChanged` events into `batch`; every other event is a no-op
/// here (the caller routes those through [`handle_lifecycle_event`]).
fn stage_lifecycle_event(
    event: &AppEvent,
    state: &mut LifecycleWatcherState,
    batch: &mut ChangedFileBatch,
) {
    if let AppEvent::FileChanged { path, .. } = event {
        stage_changed_file(state, batch, path);
    }
}

/// Accumulate a project `FileChanged` path into the work metadata of every
/// registered, still-running branch. The project file watcher carries no
/// per-session attribution, and fission branches that share the parent
/// checkout are exactly the sessions whose edits land there — so the union is
/// recorded per branch (deduplicated, first-seen-ordered by the ledger,
/// bounded by [`CHANGED_FILES_PER_BRANCH_CAP`]). Branches in isolated
/// worktrees edit outside the watch root and naturally accumulate nothing.
fn record_changed_file(state: &mut LifecycleWatcherState, path: &str) {
    let mut batch = ChangedFileBatch::default();
    stage_changed_file(state, &mut batch, path);
    batch.flush();
}

/// The per-event core of changed-file accumulation: state dedup + cap, then
/// stage (never write) the path for each still-running branch.
fn stage_changed_file(state: &mut LifecycleWatcherState, batch: &mut ChangedFileBatch, path: &str) {
    let path = path.trim();
    if path.is_empty() {
        return;
    }
    let routes: Vec<(String, BranchRoute)> = registry()
        .lock()
        .unwrap()
        .iter()
        .map(|(branch_session_id, route)| (branch_session_id.clone(), route.clone()))
        .collect();
    for (branch_session_id, route) in routes {
        if state.terminal_branches.contains(&branch_session_id) {
            continue;
        }
        let recorded = state
            .recorded_changed_files
            .entry(branch_session_id.clone())
            .or_default();
        if recorded.len() >= CHANGED_FILES_PER_BRANCH_CAP || recorded.contains(path) {
            continue;
        }
        recorded.insert(path.to_string());
        batch
            .staged
            .entry(route.log_dir)
            .or_default()
            .entry((route.group_id, branch_session_id))
            .or_default()
            .push(path.to_string());
    }
}

/// Lineage-convention summary cap (240 chars + ellipsis).
fn trim_branch_summary(value: &str) -> String {
    let value = value.trim();
    if value.chars().count() <= BRANCH_SUMMARY_MAX_CHARS {
        return value.to_string();
    }
    let mut out: String = value.chars().take(BRANCH_SUMMARY_MAX_CHARS).collect();
    out.push_str("...");
    out
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

    fn branch_snapshot(
        log_dir: &Path,
        group_id: &str,
        session: &str,
    ) -> fission_ledger::FissionBranch {
        let document = fission_ledger::read_fission_ledger_document(log_dir)
            .unwrap()
            .expect("ledger document");
        document
            .groups
            .iter()
            .find(|group| group.group_id == group_id)
            .expect("group")
            .branches
            .iter()
            .find(|branch| branch.session_id == session)
            .cloned()
            .expect("branch")
    }

    #[tokio::test]
    async fn watcher_terminal_status_survives_a_flooded_broadcast_ring() {
        let dir = tempdir().unwrap();
        let group_id = register_test_branch(
            dir.path(),
            "lw-parent-flood",
            "lw-call-flood",
            "lw-child-flood",
        );
        register_branch("lw-child-flood", &group_id, dir.path());

        let bus = crate::event::EventBus::new();
        let _watcher = spawn_fission_lifecycle_watcher(&bus);

        // Bury the terminal event in delta traffic on both sides before the
        // watcher gets a chance to run: the bounded broadcast ring (4096
        // slots) evicts it, so a broadcast subscriber would only observe
        // `Lagged` plus trailing deltas and the durable ledger would keep
        // the branch `running` forever. The lossless lane must deliver it.
        for _ in 0..10_000 {
            bus.send(AppEvent::ModelResponseDelta {
                session_id: Some("flood".to_string()),
                text: "x".to_string(),
            });
        }
        bus.send(AppEvent::DoneSignal {
            session_id: Some("lw-child-flood".to_string()),
            message: Some("flood survivor".to_string()),
        });
        for _ in 0..5_000 {
            bus.send(AppEvent::ModelResponseDelta {
                session_id: Some("flood".to_string()),
                text: "x".to_string(),
            });
        }

        let outcome = wait_for_branch_terminal(
            dir.path(),
            &group_id,
            Some("lw-child-flood"),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        let WaitOutcome::Terminal(group) = outcome else {
            panic!("branch did not reach a terminal status: {outcome:?}");
        };
        let branch = group
            .branches
            .iter()
            .find(|branch| branch.session_id == "lw-child-flood")
            .unwrap();
        assert_eq!(branch.status, "completed");
        assert_eq!(branch.summary.as_deref(), Some("flood survivor"));

        drop_pending_deliveries(&[group_id]);
    }

    #[test]
    fn watcher_maps_done_signal_to_completed_and_filters_boilerplate() {
        let dir = tempdir().unwrap();
        let group_id =
            register_test_branch(dir.path(), "lw-parent-1", "lw-call-1", "lw-child-done");
        register_branch("lw-child-done", &group_id, dir.path());
        let mut state = LifecycleWatcherState::default();

        // Boilerplate done message: completed, but no summary recorded.
        handle_lifecycle_event(
            &AppEvent::DoneSignal {
                session_id: Some("lw-child-done".to_string()),
                message: Some(DONE_SIGNAL_DEFAULT_MESSAGE.to_string()),
            },
            &mut state,
        );
        let branch = branch_snapshot(dir.path(), &group_id, "lw-child-done");
        assert_eq!(branch.status, "completed");
        assert_eq!(branch.summary, None);

        // A real done message becomes the ledger summary.
        handle_lifecycle_event(
            &AppEvent::DoneSignal {
                session_id: Some("lw-child-done".to_string()),
                message: Some("parser traced and fixed".to_string()),
            },
            &mut state,
        );
        let branch = branch_snapshot(dir.path(), &group_id, "lw-child-done");
        assert_eq!(branch.status, "completed");
        assert_eq!(branch.summary.as_deref(), Some("parser traced and fixed"));

        drop_pending_deliveries(&[group_id]);
    }

    #[test]
    fn watcher_maps_task_complete_summary_and_reason_fallback() {
        let dir = tempdir().unwrap();
        let group_id = register_test_branch(dir.path(), "lw-parent-2", "lw-call-2", "lw-child-tc");
        register_branch("lw-child-tc", &group_id, dir.path());
        let mut state = LifecycleWatcherState::default();

        handle_lifecycle_event(
            &AppEvent::TaskComplete {
                session_id: Some("lw-child-tc".to_string()),
                reason: "done".to_string(),
                summary: Some("ran the suite, all green".to_string()),
            },
            &mut state,
        );
        let branch = branch_snapshot(dir.path(), &group_id, "lw-child-tc");
        assert_eq!(branch.status, "completed");
        assert_eq!(branch.summary.as_deref(), Some("ran the suite, all green"));

        drop_pending_deliveries(&[group_id]);
    }

    #[test]
    fn watcher_interrupt_is_sticky_against_later_completion() {
        let dir = tempdir().unwrap();
        let group_id = register_test_branch(dir.path(), "lw-parent-3", "lw-call-3", "lw-child-int");
        register_branch("lw-child-int", &group_id, dir.path());
        let mut state = LifecycleWatcherState::default();

        handle_lifecycle_event(
            &AppEvent::Interrupted {
                session_id: Some("lw-child-int".to_string()),
                reason: "user stop".to_string(),
            },
            &mut state,
        );
        let branch = branch_snapshot(dir.path(), &group_id, "lw-child-int");
        assert_eq!(branch.status, "cancelled");

        // A stray later completion must not resurrect the cancelled branch
        // (sticky no-downgrade, enforced by the ledger setter).
        handle_lifecycle_event(
            &AppEvent::DoneSignal {
                session_id: Some("lw-child-int".to_string()),
                message: Some("finished anyway".to_string()),
            },
            &mut state,
        );
        let branch = branch_snapshot(dir.path(), &group_id, "lw-child-int");
        assert_eq!(branch.status, "cancelled");

        drop_pending_deliveries(&[group_id]);
    }

    #[test]
    fn watcher_session_ended_maps_failure_vs_completed_and_never_downgrades() {
        let dir = tempdir().unwrap();

        // Error-shaped teardown reason → failed, reason becomes the summary.
        let failed_group =
            register_test_branch(dir.path(), "lw-parent-4", "lw-call-4", "lw-child-fail");
        register_branch("lw-child-fail", &failed_group, dir.path());
        let mut state = LifecycleWatcherState::default();
        handle_lifecycle_event(
            &AppEvent::SessionEnded {
                session_id: "lw-child-fail".to_string(),
                reason: "error: backend exploded".to_string(),
                error_kind: None,
            },
            &mut state,
        );
        let branch = branch_snapshot(dir.path(), &failed_group, "lw-child-fail");
        assert_eq!(
            fission_ledger::normalize_branch_status(&branch.status),
            "failed"
        );
        assert_eq!(branch.summary.as_deref(), Some("error: backend exploded"));

        // Plain teardown → ended (normalizes to completed).
        let ended_group =
            register_test_branch(dir.path(), "lw-parent-5", "lw-call-5", "lw-child-end");
        register_branch("lw-child-end", &ended_group, dir.path());
        handle_lifecycle_event(
            &AppEvent::SessionEnded {
                session_id: "lw-child-end".to_string(),
                reason: "session stopped".to_string(),
                error_kind: None,
            },
            &mut state,
        );
        let branch = branch_snapshot(dir.path(), &ended_group, "lw-child-end");
        assert_eq!(branch.status, "ended");
        assert_eq!(
            fission_ledger::normalize_branch_status(&branch.status),
            "completed"
        );

        // A teardown after completion neither downgrades the status nor
        // clobbers the model-authored summary.
        let done_group =
            register_test_branch(dir.path(), "lw-parent-6", "lw-call-6", "lw-child-keep");
        register_branch("lw-child-keep", &done_group, dir.path());
        handle_lifecycle_event(
            &AppEvent::DoneSignal {
                session_id: Some("lw-child-keep".to_string()),
                message: Some("real summary".to_string()),
            },
            &mut state,
        );
        handle_lifecycle_event(
            &AppEvent::SessionEnded {
                session_id: "lw-child-keep".to_string(),
                reason: "error: late teardown".to_string(),
                error_kind: None,
            },
            &mut state,
        );
        let branch = branch_snapshot(dir.path(), &done_group, "lw-child-keep");
        assert_eq!(branch.status, "completed");
        assert_eq!(branch.summary.as_deref(), Some("real summary"));

        drop_pending_deliveries(&[failed_group, ended_group, done_group]);
    }

    #[test]
    fn watcher_ignores_unregistered_sessions() {
        let dir = tempdir().unwrap();
        let group_id =
            register_test_branch(dir.path(), "lw-parent-7", "lw-call-7", "lw-child-other");
        // Deliberately NOT registered in the lifecycle registry.
        let mut state = LifecycleWatcherState::default();
        handle_lifecycle_event(
            &AppEvent::DoneSignal {
                session_id: Some("lw-child-other".to_string()),
                message: Some("should not land".to_string()),
            },
            &mut state,
        );
        let branch = branch_snapshot(dir.path(), &group_id, "lw-child-other");
        assert_eq!(branch.status, "running");
        assert_eq!(branch.summary, None);
    }

    #[test]
    fn watcher_accumulates_changed_files_with_dedup_and_cap() {
        let dir = tempdir().unwrap();
        let group_id = register_test_branch(dir.path(), "lw-parent-8", "lw-call-8", "lw-child-fc");
        register_branch("lw-child-fc", &group_id, dir.path());
        let mut state = LifecycleWatcherState::default();

        let changed = |path: &str| AppEvent::FileChanged {
            path: path.to_string(),
            kind: crate::file_watcher::FileChangeKind::Modified,
            lines_added: 1,
            lines_removed: 0,
        };
        handle_lifecycle_event(&changed("src/lw_fc_a.rs"), &mut state);
        handle_lifecycle_event(&changed("src/lw_fc_a.rs"), &mut state);
        handle_lifecycle_event(&changed("src/lw_fc_b.rs"), &mut state);

        let document = fission_ledger::read_fission_ledger_document(dir.path())
            .unwrap()
            .expect("document");
        let ext = document
            .branch_ext(&group_id, "lw-child-fc")
            .expect("branch ext");
        assert_eq!(
            ext.changed_files
                .iter()
                .filter(|path| path.as_str() == "src/lw_fc_a.rs")
                .count(),
            1
        );
        assert!(ext
            .changed_files
            .iter()
            .any(|path| path == "src/lw_fc_b.rs"));

        // Cap: a branch with CHANGED_FILES_PER_BRANCH_CAP recorded entries
        // accepts no more.
        let recorded = state
            .recorded_changed_files
            .get_mut("lw-child-fc")
            .expect("recorded set");
        for i in recorded.len()..CHANGED_FILES_PER_BRANCH_CAP {
            recorded.insert(format!("src/lw_fc_fill_{i}.rs"));
        }
        handle_lifecycle_event(&changed("src/lw_fc_overflow.rs"), &mut state);
        let document = fission_ledger::read_fission_ledger_document(dir.path())
            .unwrap()
            .expect("document");
        let ext = document
            .branch_ext(&group_id, "lw-child-fc")
            .expect("branch ext");
        assert!(!ext
            .changed_files
            .iter()
            .any(|path| path == "src/lw_fc_overflow.rs"));

        // Terminal branches stop accumulating.
        handle_lifecycle_event(
            &AppEvent::TaskComplete {
                session_id: Some("lw-child-fc".to_string()),
                reason: "done".to_string(),
                summary: None,
            },
            &mut state,
        );
        state
            .recorded_changed_files
            .get_mut("lw-child-fc")
            .expect("recorded set")
            .clear();
        handle_lifecycle_event(&changed("src/lw_fc_post_terminal.rs"), &mut state);
        let document = fission_ledger::read_fission_ledger_document(dir.path())
            .unwrap()
            .expect("document");
        let ext = document
            .branch_ext(&group_id, "lw-child-fc")
            .expect("branch ext");
        assert!(!ext
            .changed_files
            .iter()
            .any(|path| path == "src/lw_fc_post_terminal.rs"));

        drop_pending_deliveries(&[group_id]);
    }

    #[test]
    fn changed_file_burst_batch_flushes_all_branches_in_one_write() {
        let dir = tempdir().unwrap();
        let group_a =
            register_test_branch(dir.path(), "lw-parent-9", "lw-call-9a", "lw-child-batch-a");
        let group_b =
            register_test_branch(dir.path(), "lw-parent-9", "lw-call-9b", "lw-child-batch-b");
        register_branch("lw-child-batch-a", &group_a, dir.path());
        register_branch("lw-child-batch-b", &group_b, dir.path());
        let mut state = LifecycleWatcherState::default();

        // Stage a burst of changed files (as the watcher loop's drain does),
        // then flush once: every (branch, path) pair lands.
        let mut batch = ChangedFileBatch::default();
        stage_changed_file(&mut state, &mut batch, "src/lw_batch_1.rs");
        stage_changed_file(&mut state, &mut batch, "src/lw_batch_2.rs");
        stage_changed_file(&mut state, &mut batch, "src/lw_batch_1.rs"); // dedup
        batch.flush();
        assert!(batch.staged.is_empty());

        let document = fission_ledger::read_fission_ledger_document(dir.path())
            .unwrap()
            .expect("document");
        for (group, branch) in [
            (&group_a, "lw-child-batch-a"),
            (&group_b, "lw-child-batch-b"),
        ] {
            let ext = document.branch_ext(group, branch).expect("branch ext");
            // Containment (not exact equality): the branch registry is
            // process-global, so concurrently running watcher tests may
            // interleave their own paths into these branches.
            for path in ["src/lw_batch_1.rs", "src/lw_batch_2.rs"] {
                assert_eq!(
                    ext.changed_files
                        .iter()
                        .filter(|existing| existing.as_str() == path)
                        .count(),
                    1,
                    "branch {branch} path {path}"
                );
            }
        }

        // The staged path respects terminal suppression mid-burst, exactly
        // like the immediate path.
        handle_lifecycle_event(
            &AppEvent::TaskComplete {
                session_id: Some("lw-child-batch-a".to_string()),
                reason: "done".to_string(),
                summary: None,
            },
            &mut state,
        );
        let mut batch = ChangedFileBatch::default();
        stage_changed_file(&mut state, &mut batch, "src/lw_batch_3.rs");
        batch.flush();
        let document = fission_ledger::read_fission_ledger_document(dir.path())
            .unwrap()
            .expect("document");
        assert!(!document
            .branch_ext(&group_a, "lw-child-batch-a")
            .expect("branch ext")
            .changed_files
            .iter()
            .any(|path| path == "src/lw_batch_3.rs"));
        assert!(document
            .branch_ext(&group_b, "lw-child-batch-b")
            .expect("branch ext")
            .changed_files
            .iter()
            .any(|path| path == "src/lw_batch_3.rs"));

        drop_pending_deliveries(&[group_a, group_b]);
    }

    #[test]
    fn rehydrate_registers_only_live_branches_of_attached_groups() {
        let logs_root = tempdir().unwrap();

        // Session dir A: one group with a running and a completed branch.
        let dir_a = logs_root.path().join("session-a");
        let group_a = register_test_branch(&dir_a, "rh-parent-a", "rh-call-a", "rh-child-live");
        fission_ledger::record_fission_observation(
            &dir_a,
            fission_ledger::FissionObservation {
                parent_session_id: "rh-parent-a".to_string(),
                anchor_item_id: "rh-call-a".to_string(),
                tool: "fission_spawn".to_string(),
                status: "running".to_string(),
                prompt: None,
                model: None,
                reasoning_effort: None,
                branches: vec![fission_ledger::FissionBranchObservation {
                    session_id: "rh-child-done".to_string(),
                    status: "completed".to_string(),
                    summary: None,
                }],
            },
        )
        .unwrap();

        // Session dir B: a detached group whose branches must not rehydrate.
        let dir_b = logs_root.path().join("session-b");
        let group_b = register_test_branch(&dir_b, "rh-parent-b", "rh-call-b", "rh-child-detached");
        fission_ledger::detach_group(&dir_b, &group_b, "rewind crossed anchor").unwrap();

        // Non-directory entries and dirs without ledgers are skipped.
        std::fs::create_dir_all(logs_root.path().join("empty-session")).unwrap();
        std::fs::write(logs_root.path().join("stray-file"), b"not a session dir").unwrap();

        // Make sure stale registry state cannot mask the rehydration.
        drop_pending_deliveries(&[group_a.clone(), group_b.clone()]);

        let count = rehydrate_from_logs(logs_root.path()).unwrap();
        assert_eq!(count, 1);
        let route = branch_route("rh-child-live").expect("live branch route");
        assert_eq!(route.group_id, group_a);
        assert_eq!(route.log_dir, dir_a);
        assert!(branch_route("rh-child-done").is_none());
        assert!(branch_route("rh-child-detached").is_none());

        // Missing logs root is a clean zero.
        assert_eq!(
            rehydrate_from_logs(&logs_root.path().join("missing")).unwrap(),
            0
        );

        drop_pending_deliveries(&[group_a, group_b]);
    }
}
