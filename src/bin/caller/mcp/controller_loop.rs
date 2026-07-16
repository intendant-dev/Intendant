//! Controller-restart scheduling and controller-loop/wrapper observability:
//! restart state machine + persistence, loop-status collection from run dirs,
//! external-wrapper-index and live codex-app-server process probing, halt and
//! intervention markers, and the scheduled-restart runner.

use super::*;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartAfter {
    TurnEnd,
    Now,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RestartPhase {
    AwaitingTurnComplete,
    Ready,
    Restarting,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerRestartState {
    pub restart_id: String,
    pub controller_id: String,
    pub north_star_goal: String,
    pub reason: Option<String>,
    pub restart_after: RestartAfter,
    pub phase: RestartPhase,
    pub turn_complete_token: String,
    pub handoff_summary: Option<String>,
    pub completion_status: Option<String>,
    pub restart_command: Option<String>,
    pub auto_start_task: bool,
    pub max_attempts: u32,
    pub cooldown_sec: u64,
    pub attempts: u32,
    pub created_at: String,
    pub updated_at: String,
    pub last_attempt_at: Option<String>,
    pub last_error: Option<String>,
    pub last_result: Option<String>,
}

impl ControllerRestartState {
    pub(crate) fn now_string() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    pub(crate) fn new(params: &ScheduleControllerRestartParams) -> Self {
        let now = Self::now_string();
        let restart_after = parse_restart_after(params.restart_after.as_deref())
            .expect("restart_after must be validated before creating ControllerRestartState");
        Self {
            restart_id: Uuid::new_v4().to_string(),
            controller_id: params.controller_id.clone(),
            north_star_goal: params.north_star_goal.clone(),
            reason: params.reason.clone(),
            restart_after,
            phase: match restart_after {
                RestartAfter::TurnEnd => RestartPhase::AwaitingTurnComplete,
                RestartAfter::Now => RestartPhase::Ready,
            },
            turn_complete_token: Uuid::new_v4().to_string(),
            handoff_summary: None,
            completion_status: None,
            restart_command: params.restart_command.clone(),
            auto_start_task: params.auto_start_task.unwrap_or(false),
            max_attempts: params.max_attempts.unwrap_or(1),
            cooldown_sec: params.cooldown_sec.unwrap_or(30),
            attempts: 0,
            created_at: now.clone(),
            updated_at: now,
            last_attempt_at: None,
            last_error: None,
            last_result: None,
        }
    }
}

pub(crate) fn parse_restart_after(raw: Option<&str>) -> Result<RestartAfter, String> {
    match raw.map(str::trim).map(str::to_lowercase).as_deref() {
        None | Some("") | Some("turn_end") => Ok(RestartAfter::TurnEnd),
        Some("now") => Ok(RestartAfter::Now),
        Some(other) => Err(format!(
            "Invalid request: restart_after must be 'turn_end' or 'now' (got '{}')",
            other
        )),
    }
}

pub(crate) fn normalize_string_field(value: &mut String) {
    *value = value.trim().to_string();
}

pub(crate) fn normalize_optional_string_field(value: &mut Option<String>) {
    if let Some(trimmed) = value.as_ref().map(|v| v.trim().to_string()) {
        if trimmed.is_empty() {
            *value = None;
        } else {
            *value = Some(trimmed);
        }
    }
}

pub(crate) fn normalize_schedule_controller_restart_params(
    params: &mut ScheduleControllerRestartParams,
) {
    normalize_string_field(&mut params.controller_id);
    normalize_string_field(&mut params.north_star_goal);
    normalize_optional_string_field(&mut params.reason);
    normalize_optional_string_field(&mut params.restart_after);
    if let Some(cmd) = params.restart_command.as_mut() {
        normalize_string_field(cmd);
    }
}

pub(crate) fn normalize_controller_turn_complete_params(params: &mut ControllerTurnCompleteParams) {
    normalize_string_field(&mut params.restart_id);
    normalize_string_field(&mut params.turn_complete_token);
    normalize_optional_string_field(&mut params.status);
    normalize_optional_string_field(&mut params.handoff_summary);
}

pub(crate) fn normalize_cancel_controller_restart_params(
    params: &mut CancelControllerRestartParams,
) {
    normalize_optional_string_field(&mut params.restart_id);
}

pub(crate) fn validate_schedule_controller_restart_params(
    params: &ScheduleControllerRestartParams,
) -> Result<(), String> {
    if params.controller_id.trim().is_empty() {
        return Err("Invalid request: controller_id must not be empty".to_string());
    }
    if params.north_star_goal.trim().is_empty() {
        return Err("Invalid request: north_star_goal must not be empty".to_string());
    }
    parse_restart_after(params.restart_after.as_deref())?;
    if matches!(params.max_attempts, Some(0)) {
        return Err("Invalid request: max_attempts must be >= 1".to_string());
    }
    if let Some(cmd) = params.restart_command.as_ref() {
        if cmd.trim().is_empty() {
            return Err("Invalid request: restart_command must not be empty".to_string());
        }
    }
    let has_restart_command = params
        .restart_command
        .as_ref()
        .map(|cmd| !cmd.trim().is_empty())
        .unwrap_or(false);
    let auto_start_task = params.auto_start_task.unwrap_or(false);
    if !has_restart_command && !auto_start_task {
        return Err(
            "Invalid request: configure at least one restart action (restart_command and/or auto_start_task=true)"
                .to_string(),
        );
    }
    Ok(())
}

pub(crate) fn restart_state_path(log_dir: &std::path::Path) -> std::path::PathBuf {
    log_dir.join("controller_restart.json")
}

pub(crate) fn persist_restart_state(
    log_dir: &std::path::Path,
    state: &Option<ControllerRestartState>,
) {
    let path = restart_state_path(log_dir);
    if let Some(s) = state {
        if let Ok(json) = serde_json::to_string_pretty(s) {
            let _ = std::fs::write(path, json);
        }
    } else {
        let _ = std::fs::remove_file(path);
    }
}

pub(crate) fn restart_state_public_value(
    state: Option<&ControllerRestartState>,
) -> serde_json::Value {
    let mut value = serde_json::to_value(state).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = value.as_object_mut() {
        if obj.contains_key("turn_complete_token") {
            obj.insert(
                "turn_complete_token".to_string(),
                serde_json::Value::String("[redacted]".to_string()),
            );
        }
    }
    value
}

pub(crate) fn controller_loop_dir() -> std::path::PathBuf {
    if let Ok(root) = std::env::var("INTENDANT_PROJECT_ROOT") {
        return std::path::PathBuf::from(root).join(".intendant/controller-loop");
    }
    std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join(".intendant/controller-loop")
}

pub(crate) fn read_trimmed(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn parse_pid_file(path: &std::path::Path) -> Option<u32> {
    read_trimmed(path)?.parse::<u32>().ok()
}

pub(crate) fn loop_run_dirs(loop_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut runs: Vec<std::path::PathBuf> = std::fs::read_dir(loop_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("20"))
                .unwrap_or(false)
        })
        .collect();
    runs.sort();
    runs
}

pub(crate) fn read_json_file(path: &std::path::Path) -> serde_json::Value {
    let Ok(text) = std::fs::read_to_string(path) else {
        return serde_json::Value::Null;
    };
    serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
}

pub(crate) fn intervention_order_report(run_dir: &std::path::Path) -> serde_json::Value {
    let path = run_dir.join("intervention.log");
    let Ok(text) = std::fs::read_to_string(path) else {
        return serde_json::json!({
            "has_log": false,
            "order_ok": true,
        });
    };

    let mut run_started: Option<usize> = None;
    let mut codex_started: Option<usize> = None;
    let mut cleanup_begin: Option<usize> = None;
    let mut cleanup_end: Option<usize> = None;

    for (idx, line) in text.lines().enumerate() {
        if run_started.is_none() && line.contains(" run_started ") {
            run_started = Some(idx);
        }
        if codex_started.is_none() && line.contains(" codex_started ") {
            codex_started = Some(idx);
        }
        if cleanup_begin.is_none() && line.contains(" cleanup_begin ") {
            cleanup_begin = Some(idx);
        }
        if cleanup_end.is_none() && line.contains(" cleanup_end ") {
            cleanup_end = Some(idx);
        }
    }

    let order_ok = match (run_started, codex_started, cleanup_begin, cleanup_end) {
        (Some(a), Some(b), Some(c), Some(d)) => a <= b && b <= c && c <= d,
        _ => true,
    };

    serde_json::json!({
        "has_log": true,
        "order_ok": order_ok,
        "run_started_line": run_started,
        "codex_started_line": codex_started,
        "cleanup_begin_line": cleanup_begin,
        "cleanup_end_line": cleanup_end,
    })
}

pub(crate) fn collect_controller_loop_status(loop_dir: &std::path::Path) -> serde_json::Value {
    collect_controller_loop_status_inner(loop_dir, None)
}

/// How long a [`ControllerLoopRawStatus`] snapshot stays servable from the
/// per-state cache. The raw collection spawns `ps`, scans every historical
/// run dir, and reads wrapper/session metadata — polled status surfaces
/// (`get_status`, supervised phase gates) otherwise repeat that work several
/// times per call. Enrichment against live MCP state is re-applied per
/// consumer, so only the filesystem/process sample ages, never the folded
/// session state.
pub(crate) const CONTROLLER_LOOP_RAW_STATUS_TTL: std::time::Duration =
    std::time::Duration::from_secs(1);

/// The state-independent (and expensive) half of a controller-loop status
/// collection: marker flags, lock/pid liveness, run-dir + process-tree +
/// wrapper-index discovery, latest-run pointers, and the intervention-order
/// report. Everything here is a pure filesystem/process sample, so it can be
/// collected without any `McpAppState` lock and briefly cached; the
/// state-dependent tail lives in [`finish_controller_loop_status`].
#[derive(Clone, Debug)]
pub(crate) struct ControllerLoopRawStatus {
    loop_dir: std::path::PathBuf,
    halt: bool,
    halt_after_cycle: bool,
    stop_requested: bool,
    abort_requested: bool,
    lock_present: bool,
    lock_owner_pid: Option<u32>,
    lock_owner_alive: bool,
    active_wrappers: Vec<serde_json::Value>,
    active_codex: Vec<serde_json::Value>,
    latest_run_id: Option<String>,
    latest_status_file: serde_json::Value,
    latest_target: Option<String>,
    latest_pid: Option<u32>,
    latest_pid_alive: bool,
    intervention_order: serde_json::Value,
}

impl ControllerLoopRawStatus {
    /// The loop dir this sample was collected from — the cache key.
    pub(crate) fn loop_dir(&self) -> &std::path::Path {
        &self.loop_dir
    }
}

/// `wrapper_index_home`: the home whose external-wrapper index may be
/// consulted for live codex processes (hermetic-tests convention — the
/// caller supplies the root; state-scoped callers pass
/// [`mcp_state_session_logs_home`], stateless transport edges resolve the
/// real home dir).
pub(crate) fn collect_controller_loop_raw_status(
    loop_dir: &std::path::Path,
    wrapper_index_home: &std::path::Path,
) -> ControllerLoopRawStatus {
    let halt = loop_dir.join("request_halt").exists();
    let halt_after_cycle = loop_dir.join("request_halt_after_cycle").exists();
    let stop_requested = loop_dir.join("request_stop").exists();
    let abort_requested = loop_dir.join("request_abort").exists();

    let lock_dir = loop_dir.join("active.lock");
    let lock_owner_pid = parse_pid_file(&lock_dir.join("pid"));
    let lock_owner_alive = lock_owner_pid
        .map(crate::platform::process_alive)
        .unwrap_or(false);

    let mut active_wrappers = Vec::new();
    let mut active_codex = Vec::new();
    let mut known_codex_pids = HashSet::new();
    for run in loop_run_dirs(loop_dir) {
        let run_id = run
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if let Some(pid) = parse_pid_file(&run.join("wrapper.pid")) {
            if crate::platform::process_alive(pid) {
                active_wrappers.push(serde_json::json!({
                    "run_id": run_id,
                    "pid": pid
                }));
            }
        }
        if let Some(pid) = parse_pid_file(&run.join("codex.pid")) {
            known_codex_pids.insert(pid);
            if crate::platform::process_alive(pid) {
                active_codex.push(serde_json::json!({
                    "run_id": run_id,
                    "pid": pid,
                    "source": "controller_loop",
                    "app_server_active": true,
                }));
            }
        }
    }
    let process_tree_codex =
        live_codex_app_server_process_infos(std::process::id(), &known_codex_pids);
    active_codex.extend(live_codex_app_server_processes_from_infos(
        &process_tree_codex,
    ));
    active_wrappers.extend(active_external_wrappers_from_index_for_processes(
        loop_dir,
        wrapper_index_home,
        &process_tree_codex,
    ));

    let latest_run_id = read_trimmed(&loop_dir.join("latest.run_id"));
    let latest_status_file = read_json_file(&loop_dir.join("latest.status.json"));
    let latest_target_path = std::fs::read_link(loop_dir.join("latest")).ok().map(|p| {
        if p.is_absolute() {
            p
        } else {
            loop_dir.join(p)
        }
    });
    let latest_target = latest_target_path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string());
    let latest_pid = parse_pid_file(&loop_dir.join("latest.pid"));
    let latest_pid_alive = latest_pid
        .map(crate::platform::process_alive)
        .unwrap_or(false);
    let intervention_order = latest_target_path
        .as_ref()
        .map(|p| intervention_order_report(p))
        .unwrap_or_else(|| {
            serde_json::json!({
                "has_log": false,
                "order_ok": true,
            })
        });

    ControllerLoopRawStatus {
        loop_dir: loop_dir.to_path_buf(),
        halt,
        halt_after_cycle,
        stop_requested,
        abort_requested,
        lock_present: lock_dir.exists(),
        lock_owner_pid,
        lock_owner_alive,
        active_wrappers,
        active_codex,
        latest_run_id,
        latest_status_file,
        latest_target,
        latest_pid,
        latest_pid_alive,
        intervention_order,
    }
}

/// The state-dependent tail of a controller-loop status collection: enrich
/// the discovered wrappers/codex processes with live MCP session state, fold
/// them into the latest-run status, clear stale intervention markers, and
/// assemble the public JSON document. Cheap (in-memory except for the rare
/// stale-marker unlink), so consumers of a cached [`ControllerLoopRawStatus`]
/// re-run it against current state on every read.
pub(crate) fn finish_controller_loop_status(
    raw: ControllerLoopRawStatus,
    live_state: Option<(&McpAppState, u64)>,
) -> serde_json::Value {
    let ControllerLoopRawStatus {
        loop_dir,
        halt,
        halt_after_cycle,
        mut stop_requested,
        mut abort_requested,
        lock_present,
        lock_owner_pid,
        lock_owner_alive,
        mut active_wrappers,
        mut active_codex,
        latest_run_id,
        latest_status_file,
        latest_target,
        latest_pid,
        latest_pid_alive,
        intervention_order,
    } = raw;

    if let Some((state, now_secs)) = live_state {
        enrich_controller_loop_wrappers_with_mcp_state(&mut active_wrappers, state, now_secs);
        enrich_controller_loop_codex_with_mcp_state(&mut active_codex, state, now_secs);
    }

    let latest_status = controller_loop_latest_status_with_codex(
        latest_status_file,
        &active_wrappers,
        &active_codex,
    );
    let stale_intervention_cleared = (stop_requested || abort_requested)
        && controller_loop_intervention_markers_are_stale(
            lock_owner_alive,
            latest_pid_alive,
            &active_wrappers,
            &active_codex,
        );
    if stale_intervention_cleared {
        clear_loop_intervention_markers(&loop_dir).ok();
        stop_requested = false;
        abort_requested = false;
    }

    serde_json::json!({
        "loop_dir": loop_dir.to_string_lossy(),
        "flags": {
            "halt": halt,
            "halt_after_cycle": halt_after_cycle,
            "stop_requested": stop_requested,
            "abort_requested": abort_requested,
            "stale_intervention_cleared": stale_intervention_cleared,
        },
        "lock": {
            "present": lock_present,
            "owner_pid": lock_owner_pid,
            "owner_alive": lock_owner_alive,
        },
        "latest": {
            "run_id": latest_run_id,
            "pid": latest_pid,
            "pid_alive": latest_pid_alive,
            "status": latest_status,
            "target": latest_target,
            "intervention_order": intervention_order,
        },
        "active": {
            "wrapper_count": active_wrappers.len(),
            "codex_count": active_codex.len(),
            "wrappers": active_wrappers,
            "codex": active_codex,
        }
    })
}

/// Fresh collection with live-state enrichment. Callers on marker-mutating
/// paths rely on this never serving a cached sample; it re-seeds the
/// per-state raw cache instead, so pollers observe the post-mutation loop
/// state immediately.
pub(crate) fn collect_controller_loop_status_for_mcp_state(
    loop_dir: &std::path::Path,
    state: &McpAppState,
) -> serde_json::Value {
    let (_, generation) = state.probe_controller_loop_raw_status(loop_dir);
    let raw = collect_controller_loop_raw_status(loop_dir, &mcp_state_session_logs_home(state));
    if state.controller_loop_status_override.is_none()
        && mcp_state_controller_loop_dir(state) == loop_dir
    {
        state.store_controller_loop_raw_status_at(generation, raw.clone());
    }
    finish_controller_loop_status(raw, Some((state, current_unix_timestamp_secs())))
}

pub(crate) fn collect_controller_loop_status_inner(
    loop_dir: &std::path::Path,
    live_state: Option<(&McpAppState, u64)>,
) -> serde_json::Value {
    // Stateless transport edge: no state-scoped home override exists here,
    // so the real home dir is resolved at this boundary.
    let wrapper_index_home = crate::platform::home_dir();
    finish_controller_loop_status(
        collect_controller_loop_raw_status(loop_dir, &wrapper_index_home),
        live_state,
    )
}

pub(crate) fn controller_loop_dir_has_observable_state(loop_dir: &std::path::Path) -> bool {
    loop_dir.join("active.lock").exists()
        || loop_dir.join("latest.pid").exists()
        || loop_dir.join("latest.run_id").exists()
        || loop_dir.join("latest.status.json").exists()
        || loop_dir.join("latest").exists()
        || !loop_run_dirs(loop_dir).is_empty()
}

pub(crate) fn controller_loop_status_has_live_owner_or_process(status: &serde_json::Value) -> bool {
    status
        .pointer("/lock/owner_alive")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || status
            .pointer("/latest/pid_alive")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        || status
            .pointer("/active/wrapper_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            > 0
        || status
            .pointer("/active/codex_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            > 0
}

pub(crate) fn mcp_state_session_source_for_id(
    s: &mut McpAppState,
    session_id: &str,
) -> Option<String> {
    if let Some(source) = s.session_source_for_id(session_id).map(str::to_string) {
        return Some(source);
    }
    if s.session_id == session_id {
        if let Some(source) = s.active_session_source.clone() {
            return Some(source);
        }
    }
    // The persisted fallback reads and identity-scans the whole session log;
    // a status poller for a session whose source never lands in
    // session_sources would otherwise pay that scan on every call. Memoize
    // both resolvable outcomes — resolved sources into session_sources,
    // known-non-external ids into a dedicated set. NotFound stays uncached:
    // the session's log may materialize a moment later.
    if s.session_known_non_external.contains(session_id) {
        return None;
    }
    match resolve_persisted_start_target(&mcp_state_session_logs_home(s), session_id) {
        PersistedStartTarget::External(target) => {
            s.session_sources
                .insert(session_id.to_string(), target.source.clone());
            Some(target.source)
        }
        PersistedStartTarget::ExternalMissingResume {
            source: Some(source),
        } => {
            s.session_sources
                .insert(session_id.to_string(), source.clone());
            Some(source)
        }
        // NotFound and Unreadable are both uncached: the log may appear (or
        // become readable) a moment later. Only a successfully READ log
        // with no external identity memoizes as non-external.
        PersistedStartTarget::ExternalMissingResume { source: None }
        | PersistedStartTarget::NotFound
        | PersistedStartTarget::Unreadable => None,
        PersistedStartTarget::NonExternal => {
            s.session_known_non_external.insert(session_id.to_string());
            None
        }
    }
}

pub(crate) fn mcp_state_controller_loop_dir(s: &McpAppState) -> std::path::PathBuf {
    s.controller_loop_dir_override
        .clone()
        .unwrap_or_else(controller_loop_dir)
}

pub(crate) fn mcp_state_session_logs_home(s: &McpAppState) -> std::path::PathBuf {
    s.session_logs_home_override
        .clone()
        .unwrap_or_else(crate::platform::home_dir)
}

pub(crate) fn mcp_state_controller_loop_status(s: &McpAppState) -> serde_json::Value {
    s.controller_loop_status_override
        .clone()
        .unwrap_or_else(|| {
            let loop_dir = mcp_state_controller_loop_dir(s);
            // Serve the raw filesystem/process sample from the short-TTL
            // cache when fresh — `get_status` consults this collection two
            // to three times per call (promote + active/stale checks), and
            // supervised gates poll it continuously. Enrichment always runs
            // against the live state, so folded session phases are never
            // stale even on a cache hit. Stores carry the probe generation:
            // if an invalidation (lifecycle/marker mutation) lands between
            // probe and store, the pre-mutation sample is discarded.
            let (hit, generation) = s.probe_controller_loop_raw_status(&loop_dir);
            let raw = hit.unwrap_or_else(|| {
                let raw =
                    collect_controller_loop_raw_status(&loop_dir, &mcp_state_session_logs_home(s));
                s.store_controller_loop_raw_status_at(generation, raw.clone());
                raw
            });
            finish_controller_loop_status(raw, Some((s, current_unix_timestamp_secs())))
        })
}

pub(crate) fn controller_loop_process_field_matches(
    process: &serde_json::Value,
    field: &str,
    session_ids: &std::collections::HashSet<String>,
) -> bool {
    process
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .is_some_and(|id| session_ids.contains(id))
}

pub(crate) fn controller_loop_process_matches_session(
    process: &serde_json::Value,
    session_ids: &std::collections::HashSet<String>,
) -> bool {
    [
        "intendant_session_id",
        "backend_session_id",
        "mcp_session_id",
        "session_id",
    ]
    .into_iter()
    .any(|field| controller_loop_process_field_matches(process, field, session_ids))
}

pub(crate) fn controller_loop_process_reports_active(process: &serde_json::Value) -> bool {
    process
        .get("app_server_active")
        .or_else(|| process.get("process_tree_active"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

pub(crate) fn controller_loop_status_has_active_codex_for_session(
    status: &serde_json::Value,
    session_ids: &std::collections::HashSet<String>,
) -> bool {
    ["/active/wrappers", "/active/codex"]
        .into_iter()
        .filter_map(|ptr| status.pointer(ptr).and_then(serde_json::Value::as_array))
        .flatten()
        .any(|process| {
            controller_loop_process_reports_active(process)
                && controller_loop_process_matches_session(process, session_ids)
        })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ControllerLoopCodexIdentity {
    backend_session_id: Option<String>,
    intendant_session_id: Option<String>,
    mcp_session_id: Option<String>,
    managed_context: Option<bool>,
}

pub(crate) fn string_field(value: &serde_json::Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

pub(crate) fn controller_loop_process_managed_context(process: &serde_json::Value) -> Option<bool> {
    [
        "managed_context",
        "codex_managed_context",
        "context_recovery",
    ]
    .into_iter()
    .find_map(|field| {
        process.get(field).and_then(|value| match value {
            serde_json::Value::Bool(enabled) => Some(*enabled),
            serde_json::Value::String(mode) => {
                Some(crate::project::codex_managed_context_enabled(mode))
            }
            _ => None,
        })
    })
    .or_else(|| {
        let log_path = string_field(process, "log_path")?;
        let config = crate::session_config::read_log_dir_config(std::path::Path::new(&log_path))?;
        config
            .codex_managed_context
            .as_deref()
            .map(crate::project::codex_managed_context_enabled)
    })
}

pub(crate) fn controller_loop_active_codex_identity_for_session(
    status: &serde_json::Value,
    session_ids: &std::collections::HashSet<String>,
) -> Option<ControllerLoopCodexIdentity> {
    ["/active/wrappers", "/active/codex"]
        .into_iter()
        .filter_map(|ptr| status.pointer(ptr).and_then(serde_json::Value::as_array))
        .flatten()
        .filter(|process| {
            controller_loop_process_reports_active(process)
                && controller_loop_process_matches_session(process, session_ids)
        })
        .map(|process| ControllerLoopCodexIdentity {
            backend_session_id: string_field(process, "backend_session_id"),
            intendant_session_id: string_field(process, "intendant_session_id")
                .or_else(|| string_field(process, "session_id")),
            mcp_session_id: string_field(process, "mcp_session_id"),
            managed_context: controller_loop_process_managed_context(process),
        })
        .max_by_key(|identity| {
            usize::from(identity.managed_context.is_some()) * 10
                + usize::from(identity.backend_session_id.is_some())
                + usize::from(identity.intendant_session_id.is_some())
                + usize::from(identity.mcp_session_id.is_some())
        })
}

pub(crate) fn mcp_state_promote_controller_loop_active_codex_for_session(
    s: &mut McpAppState,
    session_id: &str,
) -> bool {
    let mut session_ids = s
        .session_related_ids(session_id)
        .into_iter()
        .chain(std::iter::once(session_id.trim().to_string()))
        .filter(|id| !id.is_empty())
        .collect::<std::collections::HashSet<_>>();
    if session_ids.is_empty() {
        return false;
    }
    let status = mcp_state_controller_loop_status(s);
    let Some(identity) = controller_loop_active_codex_identity_for_session(&status, &session_ids)
    else {
        return false;
    };

    if let Some(id) = identity.backend_session_id.as_deref() {
        session_ids.insert(id.to_string());
    }
    if let Some(id) = identity.intendant_session_id.as_deref() {
        session_ids.insert(id.to_string());
    }
    if let Some(id) = identity.mcp_session_id.as_deref() {
        session_ids.insert(id.to_string());
    }
    if let (Some(wrapper_id), Some(backend_id)) = (
        identity
            .intendant_session_id
            .as_deref()
            .or(identity.mcp_session_id.as_deref()),
        identity.backend_session_id.as_deref(),
    ) {
        s.link_session_aliases(wrapper_id, backend_id);
    }
    for id in session_ids.iter().filter(|id| !id.is_empty()) {
        s.session_sources.insert(id.clone(), "codex".to_string());
    }

    let managed_context = identity
        .managed_context
        .or_else(|| {
            session_ids
                .iter()
                .find_map(|id| s.session_codex_managed_context.get(id).copied())
        })
        .unwrap_or(s.codex_managed_context || s.configured_codex_managed_context);
    for id in session_ids.iter().filter(|id| !id.is_empty()) {
        s.session_codex_managed_context
            .insert(id.clone(), managed_context);
    }
    if s.session_id.is_empty() || session_ids.iter().any(|id| id == &s.session_id) {
        s.active_session_source = Some("codex".to_string());
        s.codex_managed_context = managed_context;
    }
    true
}

pub(crate) fn mcp_state_controller_loop_has_active_codex_for_session(
    s: &McpAppState,
    session_id: &str,
) -> bool {
    let session_ids = s
        .session_related_ids(session_id)
        .into_iter()
        .chain(std::iter::once(session_id.trim().to_string()))
        .filter(|id| !id.is_empty())
        .collect::<std::collections::HashSet<_>>();
    if session_ids.is_empty() {
        return false;
    }
    let status = mcp_state_controller_loop_status(s);
    controller_loop_status_has_active_codex_for_session(&status, &session_ids)
}

pub(crate) fn mcp_state_codex_active_phase_has_stale_controller(
    s: &mut McpAppState,
    session_id: &str,
    phase: &Phase,
) -> bool {
    if !target_phase_is_active_turn(phase) {
        return false;
    }
    if !mcp_state_session_source_for_id(s, session_id)
        .as_deref()
        .is_some_and(|source| source.eq_ignore_ascii_case("codex"))
    {
        return false;
    }
    let loop_dir = mcp_state_controller_loop_dir(s);
    if !controller_loop_dir_has_observable_state(&loop_dir) {
        return false;
    }
    let status = mcp_state_controller_loop_status(s);
    !controller_loop_status_has_live_owner_or_process(&status)
}

pub(crate) fn controller_loop_intervention_markers_are_stale(
    lock_owner_alive: bool,
    latest_pid_alive: bool,
    active_wrappers: &[serde_json::Value],
    active_codex: &[serde_json::Value],
) -> bool {
    if lock_owner_alive || latest_pid_alive {
        return false;
    }
    if active_wrappers.is_empty() && !active_codex.is_empty() {
        return false;
    }
    active_wrappers
        .iter()
        .all(controller_loop_active_wrapper_is_idle_external_app_server)
}

pub(crate) fn controller_loop_active_wrapper_is_idle_external_app_server(
    wrapper: &serde_json::Value,
) -> bool {
    if wrapper.get("source").and_then(|value| value.as_str()) != Some("external_wrapper_index") {
        return false;
    }
    wrapper
        .get("session_meta_status")
        .and_then(|value| value.as_str())
        .map(controller_loop_state_is_idle)
        .unwrap_or_else(|| {
            wrapper
                .get("status")
                .and_then(|value| value.as_str())
                .map(controller_loop_state_is_idle)
                .unwrap_or(false)
        })
}

#[allow(dead_code)]
pub(crate) fn active_external_wrappers_from_index(
    loop_dir: &std::path::Path,
    wrapper_index_home: &std::path::Path,
    live_codex_pids: &[u32],
) -> Vec<serde_json::Value> {
    let live_codex_processes = live_codex_processes_from_pids(live_codex_pids);
    active_external_wrappers_from_index_for_processes(
        loop_dir,
        wrapper_index_home,
        &live_codex_processes,
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LiveCodexAppServerProcess {
    pid: u32,
    mcp_session_id: Option<String>,
}

pub(crate) fn active_external_wrappers_from_index_for_processes(
    loop_dir: &std::path::Path,
    wrapper_index_home: &std::path::Path,
    live_codex_processes: &[LiveCodexAppServerProcess],
) -> Vec<serde_json::Value> {
    let candidate_homes = controller_loop_wrapper_index_homes(loop_dir, wrapper_index_home);
    active_external_wrappers_from_index_homes_for_processes(
        candidate_homes.iter(),
        live_codex_processes,
    )
}

#[allow(dead_code)]
pub(crate) fn active_external_wrappers_from_index_homes<'a, I>(
    candidate_homes: I,
    live_codex_pids: &[u32],
) -> Vec<serde_json::Value>
where
    I: IntoIterator<Item = &'a std::path::PathBuf>,
{
    let live_codex_processes = live_codex_processes_from_pids(live_codex_pids);
    active_external_wrappers_from_index_homes_for_processes(candidate_homes, &live_codex_processes)
}

#[allow(dead_code)]
pub(crate) fn active_external_wrappers_from_index_homes_with_probe<'a, I, F>(
    candidate_homes: I,
    live_codex_pids: &[u32],
    process_tree_active: F,
) -> Vec<serde_json::Value>
where
    I: IntoIterator<Item = &'a std::path::PathBuf>,
    F: FnMut(u32) -> bool,
{
    let live_codex_processes = live_codex_processes_from_pids(live_codex_pids);
    active_external_wrappers_from_index_homes_for_processes_with_probe(
        candidate_homes,
        &live_codex_processes,
        process_tree_active,
    )
}

pub(crate) fn active_external_wrappers_from_index_homes_for_processes<'a, I>(
    candidate_homes: I,
    live_codex_processes: &[LiveCodexAppServerProcess],
) -> Vec<serde_json::Value>
where
    I: IntoIterator<Item = &'a std::path::PathBuf>,
{
    active_external_wrappers_from_index_homes_for_processes_with_probe(
        candidate_homes,
        live_codex_processes,
        codex_app_server_process_tree_active,
    )
}

pub(crate) fn active_external_wrappers_from_index_homes_for_processes_with_probe<'a, I, F>(
    candidate_homes: I,
    live_codex_processes: &[LiveCodexAppServerProcess],
    process_tree_active: F,
) -> Vec<serde_json::Value>
where
    I: IntoIterator<Item = &'a std::path::PathBuf>,
    F: FnMut(u32) -> bool,
{
    active_external_wrappers_from_index_homes_for_processes_with_probe_and_cwd(
        candidate_homes,
        live_codex_processes,
        process_tree_active,
        live_process_cwd,
    )
}

#[allow(dead_code)]
pub(crate) fn active_external_wrappers_from_index_homes_with_probe_and_cwd<'a, I, F, G>(
    candidate_homes: I,
    live_codex_pids: &[u32],
    process_tree_active: F,
    process_cwd: G,
) -> Vec<serde_json::Value>
where
    I: IntoIterator<Item = &'a std::path::PathBuf>,
    F: FnMut(u32) -> bool,
    G: FnMut(u32) -> Option<std::path::PathBuf>,
{
    let live_codex_processes = live_codex_processes_from_pids(live_codex_pids);
    active_external_wrappers_from_index_homes_for_processes_with_probe_and_cwd(
        candidate_homes,
        &live_codex_processes,
        process_tree_active,
        process_cwd,
    )
}

pub(crate) fn active_external_wrappers_from_index_homes_for_processes_with_probe_and_cwd<
    'a,
    I,
    F,
    G,
>(
    candidate_homes: I,
    live_codex_processes: &[LiveCodexAppServerProcess],
    mut process_tree_active: F,
    mut process_cwd: G,
) -> Vec<serde_json::Value>
where
    I: IntoIterator<Item = &'a std::path::PathBuf>,
    F: FnMut(u32) -> bool,
    G: FnMut(u32) -> Option<std::path::PathBuf>,
{
    if live_codex_processes.is_empty() {
        return Vec::new();
    }
    let mut seen_backend_ids = HashSet::new();
    let mut used_processes = vec![false; live_codex_processes.len()];
    let mut wrappers = Vec::new();
    for home in candidate_homes {
        for record in crate::external_wrapper_index::wrappers_for_source(home, "codex") {
            if wrappers.len() >= live_codex_processes.len() {
                break;
            }
            if seen_backend_ids.contains(&record.backend_session_id) {
                continue;
            }
            let session_meta = session_meta_snapshot(std::path::Path::new(&record.log_path));
            let status = session_meta.as_ref().and_then(|meta| meta.status.clone());
            if external_wrapper_status_is_terminal(status.as_deref()) {
                continue;
            }
            let Some((process_index, process)) =
                live_codex_processes
                    .iter()
                    .enumerate()
                    .find(|(index, process)| {
                        !used_processes[*index]
                            && live_codex_process_matches_wrapper_record(process, &record)
                    })
            else {
                continue;
            };
            used_processes[process_index] = true;
            seen_backend_ids.insert(record.backend_session_id.clone());

            let codex_pid = Some(process.pid);
            let process_tree_active = process_tree_active(process.pid);
            let effective_status =
                effective_external_wrapper_status(status.as_deref(), process_tree_active);
            let cwd = process_cwd(process.pid);
            let cwd_string = cwd.as_ref().map(|path| path.to_string_lossy().to_string());
            let project_root = cwd
                .as_deref()
                .and_then(project_root_from_process_cwd)
                .map(|path| path.to_string_lossy().to_string())
                .or_else(|| record.project_root.clone());
            let session_meta_last_turn = session_meta
                .as_ref()
                .and_then(|meta| meta.last_turn)
                .map(|turn| serde_json::Value::Number(serde_json::Number::from(turn as u64)))
                .unwrap_or(serde_json::Value::Null);
            let session_meta_rounds = session_meta
                .as_ref()
                .and_then(|meta| meta.rounds)
                .map(|rounds| serde_json::Value::Number(serde_json::Number::from(rounds as u64)))
                .unwrap_or(serde_json::Value::Null);
            let updated_at_secs =
                fresh_external_wrapper_updated_at_secs(std::path::Path::new(&record.log_path))
                    .max(record.updated_at_secs);
            wrappers.push(serde_json::json!({
                "run_id": serde_json::Value::Null,
                "pid": serde_json::Value::Null,
                "codex_pid": codex_pid,
                "app_server_pid": codex_pid,
                "app_server_active": process_tree_active,
                "source": "external_wrapper_index",
                "backend_source": record.source,
                "backend_session_id": record.backend_session_id,
                "intendant_session_id": record.intendant_session_id,
                "mcp_session_id": process.mcp_session_id.clone(),
                "log_path": record.log_path,
                "cwd": cwd_string,
                "project_root": project_root,
                "status": effective_status,
                "session_meta_status": status,
                "session_meta_last_turn": session_meta_last_turn,
                "session_meta_rounds": session_meta_rounds,
                "process_tree_active": process_tree_active,
                "updated_at_secs": updated_at_secs,
            }));
        }
        if wrappers.len() >= live_codex_processes.len() {
            break;
        }
    }
    wrappers
}

pub(crate) fn live_codex_process_matches_wrapper_record(
    process: &LiveCodexAppServerProcess,
    record: &crate::external_wrapper_index::ExternalWrapperRecord,
) -> bool {
    let Some(session_id) = process
        .mcp_session_id
        .as_deref()
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty())
    else {
        return true;
    };
    session_id == record.intendant_session_id || session_id == record.backend_session_id
}

#[allow(dead_code)]
pub(crate) fn live_codex_processes_from_pids(
    live_codex_pids: &[u32],
) -> Vec<LiveCodexAppServerProcess> {
    live_codex_pids
        .iter()
        .copied()
        .map(|pid| LiveCodexAppServerProcess {
            pid,
            mcp_session_id: None,
        })
        .collect()
}

pub(crate) fn project_root_from_process_cwd(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = Some(cwd);
    while let Some(path) = current {
        if path.join(".git").exists() {
            return Some(path.to_path_buf());
        }
        current = path.parent();
    }
    Some(cwd.to_path_buf())
}

pub(crate) fn live_process_cwd(pid: u32) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

#[allow(dead_code)]
pub(crate) fn controller_loop_latest_status(
    latest_status_file: serde_json::Value,
    wrappers: &[serde_json::Value],
) -> serde_json::Value {
    controller_loop_latest_status_with_codex(latest_status_file, wrappers, &[])
}

pub(crate) fn controller_loop_latest_status_with_codex(
    latest_status_file: serde_json::Value,
    wrappers: &[serde_json::Value],
    active_codex: &[serde_json::Value],
) -> serde_json::Value {
    let active_wrapper_status = latest_status_from_active_wrappers(wrappers);
    if let Some(status) = active_wrapper_status.as_ref().filter(|status| {
        status
            .get("live_status_source")
            .and_then(|source| source.as_str())
            == Some("mcp_state")
    }) {
        return status.clone();
    }
    let active_codex_status = latest_status_from_active_codex(active_codex);
    if let Some(status) = active_codex_status.as_ref().filter(|status| {
        status
            .get("live_status_source")
            .and_then(|source| source.as_str())
            == Some("mcp_state")
    }) {
        return status.clone();
    }
    if latest_status_file.is_null() {
        return active_wrapper_status
            .or(active_codex_status)
            .unwrap_or(serde_json::Value::Null);
    }
    if controller_loop_status_state_is_idle(&latest_status_file) {
        if let Some(status) = active_wrapper_status {
            if !controller_loop_status_state_is_idle(&status) {
                return status;
            }
        }
        if let Some(status) = active_codex_status {
            if !controller_loop_status_state_is_idle(&status) {
                return status;
            }
        }
    }
    latest_status_file
}

pub(crate) fn latest_status_from_active_wrappers(
    wrappers: &[serde_json::Value],
) -> Option<serde_json::Value> {
    let wrapper = wrappers.iter().find(|wrapper| {
        wrapper.get("source").and_then(|v| v.as_str()) == Some("external_wrapper_index")
    })?;
    let state = wrapper
        .get("phase")
        .and_then(|v| v.as_str())
        .or_else(|| wrapper.get("status").and_then(|v| v.as_str()))
        .unwrap_or("active");
    Some(serde_json::json!({
        "run_id": serde_json::Value::Null,
        "state": state,
        "pid": serde_json::Value::Null,
        "codex_pid": wrapper.get("codex_pid").cloned().unwrap_or(serde_json::Value::Null),
        "source": "external_wrapper_index",
        "backend_source": wrapper.get("backend_source").cloned().unwrap_or(serde_json::Value::Null),
        "backend_session_id": wrapper.get("backend_session_id").cloned().unwrap_or(serde_json::Value::Null),
        "intendant_session_id": wrapper.get("intendant_session_id").cloned().unwrap_or(serde_json::Value::Null),
        "mcp_session_id": wrapper.get("mcp_session_id").cloned().unwrap_or(serde_json::Value::Null),
        "log_path": wrapper.get("log_path").cloned().unwrap_or(serde_json::Value::Null),
        "session_meta_status": wrapper.get("session_meta_status").cloned().unwrap_or(serde_json::Value::Null),
        "process_tree_active": wrapper.get("process_tree_active").cloned().unwrap_or(serde_json::Value::Null),
        "app_server_pid": wrapper.get("app_server_pid").cloned().unwrap_or_else(|| wrapper.get("codex_pid").cloned().unwrap_or(serde_json::Value::Null)),
        "app_server_active": wrapper.get("app_server_active").cloned().unwrap_or_else(|| wrapper.get("process_tree_active").cloned().unwrap_or(serde_json::Value::Null)),
        "phase": wrapper.get("phase").cloned().unwrap_or(serde_json::Value::Null),
        "turn": wrapper.get("turn").cloned().unwrap_or(serde_json::Value::Null),
        "round": wrapper.get("round").cloned().unwrap_or(serde_json::Value::Null),
        "task": wrapper.get("task").cloned().unwrap_or(serde_json::Value::Null),
        "updated_at_secs": wrapper.get("updated_at_secs").cloned().unwrap_or(serde_json::Value::Null),
        "live_status_source": wrapper.get("live_status_source").cloned().unwrap_or(serde_json::Value::Null),
    }))
}

pub(crate) fn latest_status_from_active_codex(
    active_codex: &[serde_json::Value],
) -> Option<serde_json::Value> {
    let codex = active_codex
        .iter()
        .find(|codex| {
            codex
                .get("live_status_source")
                .and_then(|source| source.as_str())
                == Some("mcp_state")
        })
        .or_else(|| {
            active_codex.iter().find(|codex| {
                codex
                    .get("app_server_active")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                    && codex
                        .get("mcp_session_id")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|session_id| !session_id.trim().is_empty())
            })
        })?;
    let state = codex
        .get("phase")
        .and_then(|v| v.as_str())
        .or_else(|| codex.get("status").and_then(|v| v.as_str()))
        .unwrap_or("unknown_running");
    let pid = codex.get("pid").cloned().unwrap_or(serde_json::Value::Null);
    Some(serde_json::json!({
        "run_id": codex.get("run_id").cloned().unwrap_or(serde_json::Value::Null),
        "state": state,
        "pid": pid,
        "codex_pid": codex.get("codex_pid").cloned().unwrap_or_else(|| codex.get("pid").cloned().unwrap_or(serde_json::Value::Null)),
        "source": codex.get("source").cloned().unwrap_or(serde_json::Value::Null),
        "mcp_session_id": codex.get("mcp_session_id").cloned().unwrap_or(serde_json::Value::Null),
        "intendant_session_id": codex.get("intendant_session_id").cloned().unwrap_or_else(|| codex.get("mcp_session_id").cloned().unwrap_or(serde_json::Value::Null)),
        "backend_session_id": codex.get("backend_session_id").cloned().unwrap_or(serde_json::Value::Null),
        "app_server_pid": codex.get("app_server_pid").cloned().unwrap_or_else(|| codex.get("pid").cloned().unwrap_or(serde_json::Value::Null)),
        "app_server_active": codex.get("app_server_active").cloned().unwrap_or(serde_json::Value::Null),
        "phase": codex.get("phase").cloned().unwrap_or(serde_json::Value::Null),
        "turn": codex.get("turn").cloned().unwrap_or(serde_json::Value::Null),
        "round": codex.get("round").cloned().unwrap_or(serde_json::Value::Null),
        "task": codex.get("task").cloned().unwrap_or(serde_json::Value::Null),
        "updated_at_secs": codex.get("updated_at_secs").cloned().unwrap_or(serde_json::Value::Null),
        "live_status_source": codex.get("live_status_source").cloned().unwrap_or(serde_json::Value::Null),
    }))
}

pub(crate) async fn collect_controller_loop_status_with_state(
    loop_dir: &std::path::Path,
    state: &SharedMcpState,
) -> serde_json::Value {
    let s = state.read().await;
    collect_controller_loop_status_for_mcp_state(loop_dir, &s)
}

#[allow(dead_code)]
pub(crate) fn enrich_controller_loop_status_with_mcp_state_at(
    status: &mut serde_json::Value,
    state: &McpAppState,
    now_secs: u64,
) {
    let current_latest = status
        .pointer("/latest/status")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let latest = {
        let Some(wrappers) = status
            .pointer_mut("/active/wrappers")
            .and_then(serde_json::Value::as_array_mut)
        else {
            return;
        };

        enrich_controller_loop_wrappers_with_mcp_state(wrappers, state, now_secs);

        controller_loop_latest_status(current_latest, wrappers)
    };
    if let Some(latest_obj) = status
        .pointer_mut("/latest")
        .and_then(serde_json::Value::as_object_mut)
    {
        latest_obj.insert("status".to_string(), latest);
    }
}

pub(crate) fn enrich_controller_loop_wrappers_with_mcp_state(
    wrappers: &mut [serde_json::Value],
    state: &McpAppState,
    now_secs: u64,
) {
    for wrapper in wrappers {
        enrich_controller_loop_wrapper_with_mcp_state(wrapper, state, now_secs);
    }
}

pub(crate) fn enrich_controller_loop_codex_with_mcp_state(
    active_codex: &mut [serde_json::Value],
    state: &McpAppState,
    now_secs: u64,
) {
    for codex in active_codex {
        enrich_controller_loop_codex_process_with_mcp_state(codex, state, now_secs);
    }
}

pub(crate) fn enrich_controller_loop_codex_process_with_mcp_state(
    codex: &mut serde_json::Value,
    state: &McpAppState,
    now_secs: u64,
) {
    let Some(session_id) = codex
        .get("mcp_session_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|session_id| !session_id.is_empty())
        .map(str::to_string)
    else {
        return;
    };
    let Some(live_status) = state.session_status_for_id(&session_id).cloned() else {
        return;
    };
    let phase = phase_to_str(&live_status.phase);
    let Some(obj) = codex.as_object_mut() else {
        return;
    };
    obj.entry("intendant_session_id".to_string())
        .or_insert_with(|| serde_json::Value::String(session_id));
    obj.insert(
        "phase".to_string(),
        serde_json::Value::String(phase.to_string()),
    );
    obj.insert(
        "status".to_string(),
        serde_json::Value::String(phase.to_string()),
    );
    obj.insert(
        "turn".to_string(),
        serde_json::Value::Number(serde_json::Number::from(live_status.turn as u64)),
    );
    obj.insert(
        "round".to_string(),
        serde_json::Value::Number(serde_json::Number::from(live_status.round as u64)),
    );
    if !live_status.task.is_empty() {
        obj.insert(
            "task".to_string(),
            serde_json::Value::String(live_status.task),
        );
    }
    obj.insert(
        "updated_at_secs".to_string(),
        serde_json::Value::Number(serde_json::Number::from(now_secs)),
    );
    obj.insert(
        "live_status_source".to_string(),
        serde_json::Value::String("mcp_state".to_string()),
    );
}

pub(crate) fn enrich_controller_loop_wrapper_with_mcp_state(
    wrapper: &mut serde_json::Value,
    state: &McpAppState,
    now_secs: u64,
) {
    if wrapper.get("source").and_then(|value| value.as_str()) != Some("external_wrapper_index") {
        return;
    }
    let live_status = [
        wrapper
            .get("intendant_session_id")
            .and_then(serde_json::Value::as_str),
        wrapper
            .get("backend_session_id")
            .and_then(serde_json::Value::as_str),
    ]
    .into_iter()
    .flatten()
    .find_map(|session_id| state.session_status_for_id(session_id).cloned());
    let Some(live_status) = live_status else {
        return;
    };

    let phase = phase_to_str(&live_status.phase);
    let Some(obj) = wrapper.as_object_mut() else {
        return;
    };
    if external_wrapper_finalized_meta_wins_over_live_status(obj, &live_status) {
        let raw_meta_status = obj
            .get("session_meta_status")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        obj.entry("raw_session_meta_status".to_string())
            .or_insert(raw_meta_status.clone());
        obj.insert("phase".to_string(), raw_meta_status.clone());
        obj.insert("status".to_string(), raw_meta_status.clone());
        obj.insert("session_meta_status".to_string(), raw_meta_status);
        return;
    }
    obj.insert(
        "phase".to_string(),
        serde_json::Value::String(phase.to_string()),
    );
    obj.insert(
        "turn".to_string(),
        serde_json::Value::Number(serde_json::Number::from(live_status.turn as u64)),
    );
    obj.insert(
        "round".to_string(),
        serde_json::Value::Number(serde_json::Number::from(live_status.round as u64)),
    );
    if !live_status.task.is_empty() {
        obj.insert(
            "task".to_string(),
            serde_json::Value::String(live_status.task),
        );
    }
    obj.insert(
        "live_status_source".to_string(),
        serde_json::Value::String("mcp_state".to_string()),
    );

    if !controller_loop_phase_is_active_turn(&live_status.phase) {
        return;
    }

    let raw_meta_status = obj
        .get("session_meta_status")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    obj.entry("raw_session_meta_status".to_string())
        .or_insert(raw_meta_status);
    let wrapper_index_updated_at_secs = obj
        .get("updated_at_secs")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    obj.entry("wrapper_index_updated_at_secs".to_string())
        .or_insert(wrapper_index_updated_at_secs);
    obj.insert(
        "status".to_string(),
        serde_json::Value::String(phase.to_string()),
    );
    obj.insert(
        "session_meta_status".to_string(),
        serde_json::Value::String(phase.to_string()),
    );
    obj.insert(
        "updated_at_secs".to_string(),
        serde_json::Value::Number(serde_json::Number::from(now_secs)),
    );
}

pub(crate) fn controller_loop_phase_is_active_turn(phase: &Phase) -> bool {
    matches!(
        phase,
        Phase::Thinking
            | Phase::RunningAgent
            | Phase::Orchestrating
            | Phase::WaitingApproval
            | Phase::WaitingHuman
            | Phase::Interrupting
    )
}

pub(crate) fn external_wrapper_finalized_meta_wins_over_live_status(
    wrapper: &serde_json::Map<String, serde_json::Value>,
    live_status: &SessionStatusState,
) -> bool {
    if !controller_loop_phase_is_active_turn(&live_status.phase) {
        return false;
    }
    let Some(meta_status) = wrapper
        .get("session_meta_status")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|status| !status.is_empty())
    else {
        return false;
    };
    if external_wrapper_status_is_terminal(Some(meta_status)) {
        return true;
    }
    if !controller_loop_state_is_idle(meta_status) {
        return false;
    }
    if wrapper
        .get("session_meta_rounds")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|rounds| live_status.round <= rounds as usize)
    {
        return true;
    }
    wrapper
        .get("session_meta_last_turn")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|last_turn| live_status.turn <= last_turn as usize)
}

pub(crate) fn fresh_external_wrapper_updated_at_secs(log_dir: &std::path::Path) -> u64 {
    file_mtime_secs(&log_dir.join("session.jsonl")).max(file_mtime_secs(log_dir))
}

pub(crate) fn current_unix_timestamp_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub(crate) fn file_mtime_secs(path: &std::path::Path) -> u64 {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub(crate) fn controller_loop_home(loop_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let intendant_dir = loop_dir.parent()?;
    if intendant_dir.file_name().and_then(|name| name.to_str()) != Some(".intendant") {
        return None;
    }
    intendant_dir.parent().map(std::path::Path::to_path_buf)
}

/// Candidate homes whose external-wrapper indexes may describe the live
/// codex processes: the home the loop dir belongs to, then the caller's
/// `wrapper_index_home`. Hermetic-tests convention: the home is a
/// PARAMETER — state-scoped callers thread [`mcp_state_session_logs_home`]
/// (test-overridable) and only the stateless transport edges resolve the
/// real `home_dir()`.
pub(crate) fn controller_loop_wrapper_index_homes(
    loop_dir: &std::path::Path,
    wrapper_index_home: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    let mut homes = Vec::new();
    let mut seen = HashSet::new();
    for home in [
        controller_loop_home(loop_dir),
        Some(wrapper_index_home.to_path_buf()),
    ]
    .into_iter()
    .flatten()
    {
        if seen.insert(home.clone()) {
            homes.push(home);
        }
    }
    homes
}

#[derive(Clone, Debug)]
pub(crate) struct ExternalWrapperSessionMetaSnapshot {
    status: Option<String>,
    last_turn: Option<usize>,
    rounds: Option<usize>,
}

pub(crate) fn session_meta_snapshot(
    log_dir: &std::path::Path,
) -> Option<ExternalWrapperSessionMetaSnapshot> {
    let text = std::fs::read_to_string(log_dir.join("session_meta.json")).ok()?;
    serde_json::from_str::<crate::session_log::SessionMeta>(&text)
        .ok()
        .map(|meta| ExternalWrapperSessionMetaSnapshot {
            status: meta.status,
            last_turn: meta.last_turn,
            rounds: meta.rounds,
        })
}

pub(crate) fn external_wrapper_status_is_terminal(status: Option<&str>) -> bool {
    matches!(
        status,
        Some("completed" | "abandoned" | "interrupted" | "deleted")
    )
}

pub(crate) fn effective_external_wrapper_status(
    status: Option<&str>,
    process_tree_active: bool,
) -> String {
    let status = status.map(str::trim).filter(|status| !status.is_empty());
    if process_tree_active && status.map(controller_loop_state_is_idle).unwrap_or(true) {
        return "unknown_running".to_string();
    }
    status.unwrap_or("active").to_string()
}

pub(crate) fn controller_loop_status_state_is_idle(status: &serde_json::Value) -> bool {
    status
        .get("state")
        .or_else(|| status.get("status"))
        .and_then(|value| value.as_str())
        .map(controller_loop_state_is_idle)
        .unwrap_or(false)
}

pub(crate) fn controller_loop_state_is_idle(status: &str) -> bool {
    matches!(
        status.trim(),
        "" | "idle" | "waiting_follow_up" | "waiting_followup" | "waiting_for_task"
    )
}

pub(crate) fn codex_app_server_process_tree_active(pid: u32) -> bool {
    codex_app_server_process_tree_active_with_root(
        pid,
        // Lazy: `_with_root` never advances the descendants iterator when
        // the root is alive (the common case on this per-wrapper status
        // hot path), and materializing the descendants spawns `ps` over
        // the whole system process table — defer the spawn to first use.
        std::iter::once_with(move || crate::platform::process_descendants(pid)).flatten(),
        crate::platform::process_alive,
        crate::platform::process_cmdline,
    )
}

pub(crate) fn codex_app_server_process_tree_active_with_root<I, A, C>(
    root_pid: u32,
    descendants: I,
    mut process_alive: A,
    process_cmdline: C,
) -> bool
where
    I: IntoIterator<Item = u32>,
    A: FnMut(u32) -> bool,
    C: FnMut(u32) -> Option<String>,
{
    if process_alive(root_pid) {
        return true;
    }
    codex_app_server_process_tree_active_from_descendants(
        descendants,
        process_alive,
        process_cmdline,
    )
}

pub(crate) fn codex_app_server_process_tree_active_from_descendants<I, A, C>(
    descendants: I,
    mut process_alive: A,
    mut process_cmdline: C,
) -> bool
where
    I: IntoIterator<Item = u32>,
    A: FnMut(u32) -> bool,
    C: FnMut(u32) -> Option<String>,
{
    descendants.into_iter().any(|pid| {
        process_alive(pid)
            && process_cmdline(pid)
                .map(|cmdline| !cmdline.trim().is_empty())
                .unwrap_or(false)
    })
}

pub(crate) fn live_codex_app_server_processes_from_infos(
    processes: &[LiveCodexAppServerProcess],
) -> Vec<serde_json::Value> {
    processes
        .iter()
        .map(|process| {
            let mut entry = serde_json::json!({
                "run_id": serde_json::Value::Null,
                "pid": process.pid,
                "codex_pid": process.pid,
                "app_server_pid": process.pid,
                "source": "process_tree",
                "app_server_active": true,
            });
            if let Some(session_id) = process
                .mcp_session_id
                .as_deref()
                .map(str::trim)
                .filter(|session_id| !session_id.is_empty())
            {
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert(
                        "mcp_session_id".to_string(),
                        serde_json::Value::String(session_id.to_string()),
                    );
                    obj.insert(
                        "intendant_session_id".to_string(),
                        serde_json::Value::String(session_id.to_string()),
                    );
                }
            }
            entry
        })
        .collect()
}

pub(crate) fn live_codex_app_server_process_infos(
    root_pid: u32,
    known_codex_pids: &HashSet<u32>,
) -> Vec<LiveCodexAppServerProcess> {
    live_codex_app_server_process_infos_from_descendants(
        crate::platform::process_descendants(root_pid),
        known_codex_pids,
        crate::platform::process_cmdline,
    )
}

pub(crate) fn live_codex_app_server_pids(
    root_pid: u32,
    known_codex_pids: &HashSet<u32>,
) -> Vec<u32> {
    live_codex_app_server_process_infos(root_pid, known_codex_pids)
        .into_iter()
        .map(|process| process.pid)
        .collect()
}

#[allow(dead_code)]
pub(crate) fn live_codex_app_server_pids_from_descendants<I, F>(
    descendant_pids: I,
    known_codex_pids: &HashSet<u32>,
    mut cmdline_for_pid: F,
) -> Vec<u32>
where
    I: IntoIterator<Item = u32>,
    F: FnMut(u32) -> Option<String>,
{
    live_codex_app_server_process_infos_from_descendants(
        descendant_pids,
        known_codex_pids,
        &mut cmdline_for_pid,
    )
    .into_iter()
    .map(|process| process.pid)
    .collect()
}

pub(crate) fn live_codex_app_server_process_infos_from_descendants<I, F>(
    descendant_pids: I,
    known_codex_pids: &HashSet<u32>,
    mut cmdline_for_pid: F,
) -> Vec<LiveCodexAppServerProcess>
where
    I: IntoIterator<Item = u32>,
    F: FnMut(u32) -> Option<String>,
{
    let mut processes = Vec::new();
    for pid in descendant_pids {
        if known_codex_pids.contains(&pid) {
            continue;
        }
        let Some(cmdline) = cmdline_for_pid(pid) else {
            continue;
        };
        if is_codex_app_server_cmdline(&cmdline) {
            processes.push(LiveCodexAppServerProcess {
                pid,
                mcp_session_id: codex_app_server_mcp_session_id_from_cmdline(&cmdline),
            });
        }
    }
    processes.sort_by_key(|process| process.pid);
    processes.dedup_by_key(|process| process.pid);
    processes
}

pub(crate) fn is_codex_app_server_cmdline(cmdline: &str) -> bool {
    let mut args = cmdline.split_whitespace();
    args.any(|arg| arg.ends_with("codex")) && args.any(|arg| arg == "app-server")
}

pub(crate) fn codex_app_server_mcp_session_id_from_cmdline(cmdline: &str) -> Option<String> {
    const URL_KEY: &str = "mcp_servers.intendant.url=";
    cmdline.split_whitespace().find_map(|arg| {
        let raw_url = arg
            .strip_prefix(URL_KEY)
            .or_else(|| arg.find(URL_KEY).map(|index| &arg[index + URL_KEY.len()..]))?;
        mcp_session_id_from_url_value(raw_url)
    })
}

pub(crate) fn mcp_session_id_from_url_value(raw_url: &str) -> Option<String> {
    let url = raw_url
        .trim()
        .trim_matches(|ch| ch == '"' || ch == '\'' || ch == '\\');
    let (_, query) = url.split_once('?')?;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if matches!(key, "session_id" | "session" | "intendant_session") {
            let decoded = percent_decode_mcp_query_value(value);
            if !decoded.trim().is_empty() {
                return Some(decoded);
            }
        }
    }
    None
}

pub(crate) fn percent_decode_mcp_query_value(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (
                hex_mcp_query_value(bytes[i + 1]),
                hex_mcp_query_value(bytes[i + 2]),
            ) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub(crate) fn hex_mcp_query_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(crate) fn request_loop_halt_marker(
    loop_dir: &std::path::Path,
    persistent: bool,
) -> Result<(), String> {
    std::fs::create_dir_all(loop_dir).map_err(|e| format!("Failed to create loop dir: {}", e))?;
    if persistent {
        std::fs::write(loop_dir.join("request_halt"), b"")
            .map_err(|e| format!("Failed to write request_halt: {}", e))?;
    } else {
        std::fs::write(loop_dir.join("request_halt_after_cycle"), b"")
            .map_err(|e| format!("Failed to write request_halt_after_cycle: {}", e))?;
    }
    Ok(())
}

pub(crate) fn clear_loop_halt_markers(loop_dir: &std::path::Path) -> Result<(), String> {
    std::fs::remove_file(loop_dir.join("request_halt")).ok();
    std::fs::remove_file(loop_dir.join("request_halt_after_cycle")).ok();
    clear_loop_intervention_markers(loop_dir)?;
    Ok(())
}

pub(crate) fn clear_loop_intervention_markers(loop_dir: &std::path::Path) -> Result<(), String> {
    std::fs::remove_file(loop_dir.join("request_stop")).ok();
    std::fs::remove_file(loop_dir.join("request_abort")).ok();
    Ok(())
}

pub(crate) fn normalize_intervention_mode(mode: &str) -> String {
    mode.trim().to_lowercase()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControllerLoopInterventionMode {
    Stop,
    Abort,
}

impl ControllerLoopInterventionMode {
    pub(crate) fn parse(mode: &str) -> Result<Self, String> {
        match normalize_intervention_mode(mode).as_str() {
            "stop" => Ok(Self::Stop),
            "abort" => Ok(Self::Abort),
            other => Err(format!(
                "Invalid mode '{}': expected 'stop' or 'abort'",
                other
            )),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Abort => "abort",
        }
    }

    pub(crate) fn marker_name(self) -> &'static str {
        match self {
            Self::Stop => "request_stop",
            Self::Abort => "request_abort",
        }
    }

    pub(crate) fn process_signal(self) -> crate::platform::ProcessSignal {
        match self {
            Self::Stop => crate::platform::ProcessSignal::Terminate,
            Self::Abort => crate::platform::ProcessSignal::Kill,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ControllerLoopIntervention {
    pub(crate) mode: ControllerLoopInterventionMode,
    pub(crate) signaled_codex_app_server_pids: Vec<u32>,
}

pub(crate) fn request_loop_intervention_marker(
    loop_dir: &std::path::Path,
    mode: &str,
) -> Result<ControllerLoopIntervention, String> {
    request_loop_intervention_marker_for_root(loop_dir, mode, std::process::id())
}

pub(crate) fn request_loop_intervention_marker_for_root(
    loop_dir: &std::path::Path,
    mode: &str,
    root_pid: u32,
) -> Result<ControllerLoopIntervention, String> {
    std::fs::create_dir_all(loop_dir).map_err(|e| format!("Failed to create loop dir: {}", e))?;
    let mode = ControllerLoopInterventionMode::parse(mode)?;
    let marker_name = mode.marker_name();
    std::fs::write(loop_dir.join(marker_name), b"")
        .map_err(|e| format!("Failed to write {}: {}", marker_name, e))?;

    let signaled_codex_app_server_pids = signal_live_codex_app_server_processes(root_pid, mode);
    Ok(ControllerLoopIntervention {
        mode,
        signaled_codex_app_server_pids,
    })
}

pub(crate) fn signal_live_codex_app_server_processes(
    root_pid: u32,
    mode: ControllerLoopInterventionMode,
) -> Vec<u32> {
    let known_codex_pids = HashSet::new();
    let pids = live_codex_app_server_pids(root_pid, &known_codex_pids);
    for pid in &pids {
        let _ = crate::platform::signal_process_tree_now(*pid, mode.process_signal());
    }
    pids
}

pub(crate) fn controller_loop_intervention_report(
    intervention: &ControllerLoopIntervention,
) -> serde_json::Value {
    serde_json::json!({
        "mode": intervention.mode.as_str(),
        "signaled_codex_app_server_count": intervention.signaled_codex_app_server_pids.len(),
        "signaled_codex_app_server_pids": &intervention.signaled_codex_app_server_pids,
    })
}

pub(crate) fn add_controller_loop_intervention_report(
    status: &mut serde_json::Value,
    intervention: &ControllerLoopIntervention,
) {
    if let Some(obj) = status.as_object_mut() {
        obj.insert(
            "intervention".to_string(),
            controller_loop_intervention_report(intervention),
        );
    }
}

pub(crate) async fn spawn_detached_restart_command(cmd: &str) -> Result<u32, String> {
    // Delegate to the platform helper: `nohup setsid bash -lc` on Unix
    // (unchanged), a detached window-less `cmd.exe /C` child on Windows.
    crate::platform::spawn_detached_restart(cmd).await
}

pub(crate) async fn run_scheduled_controller_restart_with_state(
    state: &SharedMcpState,
    bus: &EventBus,
) -> Result<String, String> {
    let (restart, log_dir) = {
        let mut s = state.write().await;
        let log_dir = s.log_dir.clone();
        let Some(active) = s.controller_restart.as_mut() else {
            return Err("No scheduled controller restart".to_string());
        };

        if !matches!(active.phase, RestartPhase::Ready) {
            return Err(format!(
                "Restart is not ready (current phase: {:?})",
                active.phase
            ));
        }

        if active.attempts >= active.max_attempts {
            active.phase = RestartPhase::Failed;
            active.last_error = Some("Max restart attempts reached".to_string());
            active.updated_at = ControllerRestartState::now_string();
            let snapshot = s.controller_restart.clone();
            persist_restart_state(&log_dir, &snapshot);
            return Err("Max restart attempts reached".to_string());
        }

        if let Some(last_attempt) = &active.last_attempt_at {
            if let Ok(last) = chrono::DateTime::parse_from_rfc3339(last_attempt) {
                let elapsed = chrono::Utc::now() - last.with_timezone(&chrono::Utc);
                if elapsed.num_seconds() < active.cooldown_sec as i64 {
                    return Err(format!(
                        "Restart cooldown active ({}s remaining)",
                        active
                            .cooldown_sec
                            .saturating_sub(elapsed.num_seconds() as u64)
                    ));
                }
            }
        }

        active.phase = RestartPhase::Restarting;
        active.attempts += 1;
        active.last_attempt_at = Some(ControllerRestartState::now_string());
        active.updated_at = ControllerRestartState::now_string();
        active.last_error = None;
        active.last_result = None;
        let restart = active.clone();
        let snapshot = s.controller_restart.clone();
        persist_restart_state(&log_dir, &snapshot);
        (restart, log_dir)
    };

    let mut result_parts = Vec::new();

    if let Some(cmd) = &restart.restart_command {
        match spawn_detached_restart_command(cmd).await {
            Ok(pid) => {
                result_parts.push(format!("spawned controller command (pid {})", pid));
            }
            Err(e) => {
                let mut s = state.write().await;
                if let Some(active) = s.controller_restart.as_mut() {
                    active.phase = RestartPhase::Failed;
                    active.last_error = Some(format!("Failed to spawn restart_command: {}", e));
                    active.updated_at = ControllerRestartState::now_string();
                }
                let snapshot = s.controller_restart.clone();
                persist_restart_state(&log_dir, &snapshot);
                return Err(format!("Failed to spawn restart_command: {}", e));
            }
        }
    }

    if restart.auto_start_task {
        if let Err(e) = start_task_with_state(
            state,
            bus,
            restart.north_star_goal.clone(),
            "controller_restart",
            None, // auto-start uses default mode selection
        )
        .await
        {
            let failure = format!("Failed to start follow-up task: {}", e);
            let mut s = state.write().await;
            if let Some(active) = s.controller_restart.as_mut() {
                active.phase = RestartPhase::Failed;
                active.last_error = Some(failure.clone());
                active.updated_at = ControllerRestartState::now_string();
            }
            let snapshot = s.controller_restart.clone();
            persist_restart_state(&log_dir, &snapshot);
            return Err(failure);
        }
        result_parts.push("started autonomous follow-up task".to_string());
    }

    if restart.restart_command.is_none() && !restart.auto_start_task {
        let mut s = state.write().await;
        if let Some(active) = s.controller_restart.as_mut() {
            active.phase = RestartPhase::Failed;
            active.last_error = Some(
                "No restart action configured: set restart_command and/or auto_start_task=true"
                    .to_string(),
            );
            active.updated_at = ControllerRestartState::now_string();
        }
        let snapshot = s.controller_restart.clone();
        persist_restart_state(&log_dir, &snapshot);
        return Err("No restart action configured".to_string());
    }

    let mut s = state.write().await;
    if let Some(active) = s.controller_restart.as_mut() {
        active.phase = RestartPhase::Completed;
        active.last_result = Some(if result_parts.is_empty() {
            "ok".to_string()
        } else {
            result_parts.join("; ")
        });
        active.updated_at = ControllerRestartState::now_string();
    }
    let snapshot = s.controller_restart.clone();
    persist_restart_state(&log_dir, &snapshot);

    Ok(result_parts.join("; "))
}

pub(crate) fn restart_phase_value(state: &ControllerRestartState) -> serde_json::Value {
    serde_json::to_value(state.phase).unwrap_or(serde_json::Value::Null)
}

pub(crate) fn restart_error_response(
    status: &str,
    restart_id: &str,
    phase: Option<RestartPhase>,
    error: String,
) -> String {
    let mut output = serde_json::json!({
        "status": status,
        "restart_id": restart_id,
        "ok": false,
        "error": error,
    });
    if let Some(phase) = phase {
        output["phase"] = serde_json::to_value(phase).unwrap_or(serde_json::Value::Null);
    }
    serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
}

pub(crate) fn schedule_error_response(
    error: String,
    restart_id: Option<&str>,
    phase: Option<RestartPhase>,
) -> String {
    let mut output = serde_json::json!({
        "status": "rejected",
        "ok": false,
        "error": error,
    });
    if let Some(restart_id) = restart_id {
        output["restart_id"] = serde_json::Value::String(restart_id.to_string());
    }
    if let Some(phase) = phase {
        output["phase"] = serde_json::to_value(phase).unwrap_or(serde_json::Value::Null);
    }
    serde_json::to_string_pretty(&output).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::{self, AutonomyState};
    use crate::mcp::tests::test_state_with_log_dir;
    use tempfile::tempdir;

    #[test]
    fn parse_restart_after_defaults_to_turn_end() {
        assert_eq!(parse_restart_after(None).unwrap(), RestartAfter::TurnEnd);
        assert_eq!(
            parse_restart_after(Some("turn_end")).unwrap(),
            RestartAfter::TurnEnd
        );
        assert_eq!(parse_restart_after(Some("NOW")).unwrap(), RestartAfter::Now);
    }

    #[test]
    fn parse_restart_after_rejects_invalid_value() {
        let err = parse_restart_after(Some("later")).unwrap_err();
        assert!(err.contains("restart_after must be 'turn_end' or 'now'"));
    }

    #[test]
    fn normalize_optional_string_field_trims_and_drops_empty() {
        let mut value = Some("  hello  ".to_string());
        normalize_optional_string_field(&mut value);
        assert_eq!(value.as_deref(), Some("hello"));

        let mut empty = Some("   ".to_string());
        normalize_optional_string_field(&mut empty);
        assert!(empty.is_none());
    }

    #[test]
    fn controller_restart_state_defaults() {
        let params = ScheduleControllerRestartParams {
            controller_id: "codex".to_string(),
            north_star_goal: "audit and improve".to_string(),
            reason: Some("periodic refresh".to_string()),
            restart_after: None,
            restart_command: None,
            auto_start_task: None,
            max_attempts: None,
            cooldown_sec: None,
        };
        let state = ControllerRestartState::new(&params);
        assert_eq!(state.controller_id, "codex");
        assert_eq!(state.phase, RestartPhase::AwaitingTurnComplete);
        assert_eq!(state.max_attempts, 1);
        assert_eq!(state.cooldown_sec, 30);
        assert!(!state.auto_start_task);
    }

    #[test]
    fn restart_state_public_value_redacts_turn_complete_token() {
        let params = ScheduleControllerRestartParams {
            controller_id: "codex".to_string(),
            north_star_goal: "audit and improve".to_string(),
            reason: None,
            restart_after: None,
            restart_command: Some("true".to_string()),
            auto_start_task: Some(false),
            max_attempts: None,
            cooldown_sec: None,
        };
        let restart = ControllerRestartState::new(&params);
        let raw_token = restart.turn_complete_token.clone();

        let public = restart_state_public_value(Some(&restart));
        assert_eq!(
            public.get("turn_complete_token").and_then(|v| v.as_str()),
            Some("[redacted]")
        );
        assert_ne!(
            public.get("turn_complete_token").and_then(|v| v.as_str()),
            Some(raw_token.as_str())
        );
        assert_eq!(
            public.get("restart_id").and_then(|v| v.as_str()),
            Some(restart.restart_id.as_str())
        );
    }

    #[test]
    fn controller_loop_halt_markers_roundtrip() {
        let dir = tempdir().unwrap();
        let loop_dir = dir.path().join(".intendant/controller-loop");

        request_loop_halt_marker(&loop_dir, true).expect("persistent halt should succeed");
        assert!(loop_dir.join("request_halt").exists());

        request_loop_halt_marker(&loop_dir, false).expect("one-shot halt should succeed");
        assert!(loop_dir.join("request_halt_after_cycle").exists());

        clear_loop_halt_markers(&loop_dir).expect("clear halt should succeed");
        assert!(!loop_dir.join("request_halt").exists());
        assert!(!loop_dir.join("request_halt_after_cycle").exists());
    }

    #[test]
    fn controller_loop_clear_halt_resets_stop_marker_status() {
        let dir = tempdir().unwrap();
        let loop_dir = dir.path().join(".intendant/controller-loop");
        let intervention =
            request_loop_intervention_marker_for_root(&loop_dir, "stop", u32::MAX).unwrap();
        assert_eq!(intervention.mode, ControllerLoopInterventionMode::Stop);
        assert!(loop_dir.join("request_stop").exists());

        clear_loop_halt_markers(&loop_dir).expect("clear halt should succeed");

        assert!(!loop_dir.join("request_stop").exists());
        assert_eq!(
            collect_controller_loop_status(&loop_dir)
                .get("flags")
                .and_then(|v| v.get("stop_requested"))
                .and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn controller_loop_status_clears_stale_intervention_markers_without_active_owner() {
        let dir = tempdir().unwrap();
        let loop_dir = dir.path().join(".intendant/controller-loop");
        std::fs::create_dir_all(&loop_dir).unwrap();
        std::fs::write(loop_dir.join("request_stop"), b"").unwrap();
        std::fs::write(loop_dir.join("request_abort"), b"").unwrap();

        let status = collect_controller_loop_status(&loop_dir);

        assert!(!loop_dir.join("request_stop").exists());
        assert!(!loop_dir.join("request_abort").exists());
        assert_eq!(
            status
                .get("flags")
                .and_then(|v| v.get("stop_requested"))
                .and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            status
                .get("flags")
                .and_then(|v| v.get("abort_requested"))
                .and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            status
                .get("flags")
                .and_then(|v| v.get("stale_intervention_cleared"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn controller_loop_intervention_markers_are_stale_for_idle_external_app_server_wrapper() {
        let idle_wrapper = serde_json::json!({
            "source": "external_wrapper_index",
            "status": "unknown_running",
            "session_meta_status": "idle",
            "process_tree_active": true,
            "app_server_active": true,
        });
        let running_wrapper = serde_json::json!({
            "source": "external_wrapper_index",
            "status": "unknown_running",
            "session_meta_status": "running",
            "process_tree_active": true,
            "app_server_active": true,
        });
        let controller_loop_wrapper = serde_json::json!({
            "source": "controller_loop",
            "pid": std::process::id(),
        });

        assert!(controller_loop_intervention_markers_are_stale(
            false,
            false,
            &[idle_wrapper],
            &[serde_json::json!({"pid": 8894})]
        ));
        assert!(!controller_loop_intervention_markers_are_stale(
            false,
            false,
            &[running_wrapper],
            &[serde_json::json!({"pid": 8894})]
        ));
        assert!(!controller_loop_intervention_markers_are_stale(
            false,
            false,
            &[controller_loop_wrapper],
            &[]
        ));
        assert!(!controller_loop_intervention_markers_are_stale(
            false,
            false,
            &[],
            &[serde_json::json!({"pid": 8894})]
        ));
        assert!(!controller_loop_intervention_markers_are_stale(
            true,
            false,
            &[],
            &[]
        ));
        assert!(!controller_loop_intervention_markers_are_stale(
            false,
            true,
            &[],
            &[]
        ));
    }

    #[test]
    fn controller_loop_status_reports_live_pid_counts() {
        let dir = tempdir().unwrap();
        let loop_dir = dir.path().join(".intendant/controller-loop");
        let run_dir = loop_dir.join("20260101T000000Z-1234");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("wrapper.pid"), std::process::id().to_string()).unwrap();
        std::fs::write(run_dir.join("codex.pid"), std::process::id().to_string()).unwrap();
        std::fs::write(loop_dir.join("latest.run_id"), "20260101T000000Z-1234").unwrap();
        std::fs::write(
            loop_dir.join("latest.status.json"),
            r#"{"run_id":"20260101T000000Z-1234","state":"running"}"#,
        )
        .unwrap();

        let status = collect_controller_loop_status(&loop_dir);
        assert_eq!(
            status
                .get("active")
                .and_then(|v| v.get("wrapper_count"))
                .and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            status
                .get("active")
                .and_then(|v| v.get("codex_count"))
                .and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            status
                .get("latest")
                .and_then(|v| v.get("run_id"))
                .and_then(|v| v.as_str()),
            Some("20260101T000000Z-1234")
        );
    }

    #[test]
    fn controller_loop_status_enriches_live_app_server_from_wrapper_index() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let loop_dir = home.join(".intendant/controller-loop");
        let log_dir = home.join(".intendant/logs/wrapper-session");
        std::fs::create_dir_all(&loop_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrapper-session",
                "created_at": "2026-01-01T00:00:00Z",
                "status": "running"
            })
            .to_string(),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            home,
            "codex",
            "3addb0e1-b533-4836-8165-d8ad0c198e4b",
            "wrapper-session",
            &log_dir,
            None,
        )
        .unwrap();

        let wrappers = active_external_wrappers_from_index(&loop_dir, home, &[1084559]);
        assert_eq!(wrappers.len(), 1);
        assert_eq!(
            wrappers[0]
                .get("backend_session_id")
                .and_then(|value| value.as_str()),
            Some("3addb0e1-b533-4836-8165-d8ad0c198e4b")
        );
        assert_eq!(
            wrappers[0]
                .get("intendant_session_id")
                .and_then(|value| value.as_str()),
            Some("wrapper-session")
        );
        assert_eq!(
            wrappers[0].get("status").and_then(|value| value.as_str()),
            Some("running")
        );
        let latest = latest_status_from_active_wrappers(&wrappers).unwrap();
        assert_eq!(
            latest
                .get("backend_session_id")
                .and_then(|value| value.as_str()),
            Some("3addb0e1-b533-4836-8165-d8ad0c198e4b")
        );
        assert_eq!(
            latest.get("state").and_then(|value| value.as_str()),
            Some("running")
        );
    }

    #[test]
    fn controller_loop_status_does_not_report_idle_for_active_codex_process_tree() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let loop_dir = home.join(".intendant/controller-loop");
        let log_dir = home.join(".intendant/logs/wrapper-session");
        std::fs::create_dir_all(&loop_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrapper-session",
                "created_at": "2026-01-01T00:00:00Z",
                "status": "idle"
            })
            .to_string(),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            home,
            "codex",
            "019e9b9a-8557-7b01-99ef-187e8840327f",
            "wrapper-session",
            &log_dir,
            None,
        )
        .unwrap();

        let wrappers = active_external_wrappers_from_index_homes_with_probe(
            [home.to_path_buf()].iter(),
            &[8892],
            |pid| pid == 8892,
        );

        assert_eq!(wrappers.len(), 1);
        assert_eq!(
            wrappers[0].get("status").and_then(|value| value.as_str()),
            Some("unknown_running")
        );
        assert_eq!(
            wrappers[0]
                .get("session_meta_status")
                .and_then(|value| value.as_str()),
            Some("idle")
        );
        assert_eq!(
            wrappers[0]
                .get("process_tree_active")
                .and_then(|value| value.as_bool()),
            Some(true)
        );

        let latest = controller_loop_latest_status(
            serde_json::json!({
                "run_id": "stale-run",
                "state": "idle"
            }),
            &wrappers,
        );
        assert_eq!(
            latest.get("state").and_then(|value| value.as_str()),
            Some("unknown_running")
        );
        assert_eq!(
            latest.get("source").and_then(|value| value.as_str()),
            Some("external_wrapper_index")
        );
        assert_eq!(
            latest
                .get("session_meta_status")
                .and_then(|value| value.as_str()),
            Some("idle")
        );
        assert_eq!(
            latest
                .get("process_tree_active")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn controller_loop_status_enriches_index_wrapper_from_live_mcp_state() {
        let mut app_state = McpAppState::new(
            "openai".to_string(),
            "gpt-5.2-codex".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test-session"),
        );
        app_state.link_session_aliases("wrapper-session", "codex-thread");
        app_state.note_session_phase(
            Some("codex-thread"),
            Some(14),
            Phase::RunningAgent,
            Some("Codex follow-up round 14 in progress: fix the controller status"),
        );
        app_state.note_session_round(Some("codex-thread"), 14);

        let mut status = serde_json::json!({
            "latest": {
                "status": {
                    "run_id": "stale-run",
                    "state": "idle"
                }
            },
            "active": {
                "wrappers": [{
                    "run_id": null,
                    "pid": null,
                    "codex_pid": 8892,
                    "app_server_pid": 8892,
                    "app_server_active": true,
                    "source": "external_wrapper_index",
                    "backend_source": "codex",
                    "backend_session_id": "codex-thread",
                    "intendant_session_id": "wrapper-session",
                    "log_path": "/tmp/test-session",
                    "status": "unknown_running",
                    "session_meta_status": "idle",
                    "process_tree_active": true,
                    "updated_at_secs": 10
                }]
            }
        });

        enrich_controller_loop_status_with_mcp_state_at(&mut status, &app_state, 12345);

        let wrapper = status.pointer("/active/wrappers/0").unwrap();
        assert_eq!(
            wrapper.get("status").and_then(|value| value.as_str()),
            Some("running_agent")
        );
        assert_eq!(
            wrapper
                .get("session_meta_status")
                .and_then(|value| value.as_str()),
            Some("running_agent")
        );
        assert_eq!(
            wrapper
                .get("raw_session_meta_status")
                .and_then(|value| value.as_str()),
            Some("idle")
        );
        assert_eq!(
            wrapper.get("turn").and_then(|value| value.as_u64()),
            Some(14)
        );
        assert_eq!(
            wrapper.get("round").and_then(|value| value.as_u64()),
            Some(14)
        );
        assert_eq!(
            wrapper
                .get("updated_at_secs")
                .and_then(|value| value.as_u64()),
            Some(12345)
        );
        assert_eq!(
            status
                .pointer("/latest/status/state")
                .and_then(|value| value.as_str()),
            Some("running_agent")
        );
        assert_eq!(
            status
                .pointer("/latest/status/turn")
                .and_then(|value| value.as_u64()),
            Some(14)
        );
        assert!(!controller_loop_intervention_markers_are_stale(
            false,
            false,
            &[wrapper.clone()],
            &[serde_json::json!({"pid": 8892})]
        ));
    }

    #[test]
    fn controller_loop_status_keeps_finalized_wrapper_idle_over_stale_live_mcp_phase() {
        let mut app_state = McpAppState::new(
            "openai".to_string(),
            "gpt-5.2-codex".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test-session"),
        );
        app_state.link_session_aliases("wrapper-session", "codex-thread");
        app_state.note_session_phase(
            Some("codex-thread"),
            Some(14),
            Phase::Thinking,
            Some("stale Codex follow-up round 14 thinking"),
        );
        app_state.note_session_round(Some("codex-thread"), 14);

        let mut status = serde_json::json!({
            "latest": {
                "status": {
                    "run_id": "stale-run",
                    "state": "idle"
                }
            },
            "active": {
                "wrappers": [{
                    "run_id": null,
                    "pid": null,
                    "codex_pid": 8892,
                    "app_server_pid": 8892,
                    "app_server_active": true,
                    "source": "external_wrapper_index",
                    "backend_source": "codex",
                    "backend_session_id": "codex-thread",
                    "intendant_session_id": "wrapper-session",
                    "log_path": "/tmp/test-session",
                    "status": "unknown_running",
                    "session_meta_status": "idle",
                    "session_meta_last_turn": 14,
                    "session_meta_rounds": 14,
                    "process_tree_active": true,
                    "updated_at_secs": 10
                }]
            }
        });

        enrich_controller_loop_status_with_mcp_state_at(&mut status, &app_state, 12345);

        let wrapper = status.pointer("/active/wrappers/0").unwrap();
        assert_eq!(
            wrapper.get("phase").and_then(|value| value.as_str()),
            Some("idle")
        );
        assert_eq!(
            wrapper.get("status").and_then(|value| value.as_str()),
            Some("idle")
        );
        assert_eq!(
            wrapper
                .get("session_meta_status")
                .and_then(|value| value.as_str()),
            Some("idle")
        );
        assert_eq!(
            wrapper
                .get("raw_session_meta_status")
                .and_then(|value| value.as_str()),
            Some("idle")
        );
        assert_eq!(
            status
                .pointer("/latest/status/state")
                .and_then(|value| value.as_str()),
            Some("idle")
        );
    }

    #[test]
    fn controller_loop_status_reports_new_live_round_over_prior_idle_meta() {
        let mut app_state = McpAppState::new(
            "openai".to_string(),
            "gpt-5.2-codex".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test-session"),
        );
        app_state.link_session_aliases("wrapper-session", "codex-thread");
        app_state.note_session_phase(
            Some("codex-thread"),
            Some(15),
            Phase::RunningAgent,
            Some("Codex follow-up round 15 in progress"),
        );
        app_state.note_session_round(Some("codex-thread"), 15);

        let mut status = serde_json::json!({
            "latest": {
                "status": {
                    "run_id": "stale-run",
                    "state": "idle"
                }
            },
            "active": {
                "wrappers": [{
                    "run_id": null,
                    "pid": null,
                    "codex_pid": 8892,
                    "app_server_pid": 8892,
                    "app_server_active": true,
                    "source": "external_wrapper_index",
                    "backend_source": "codex",
                    "backend_session_id": "codex-thread",
                    "intendant_session_id": "wrapper-session",
                    "log_path": "/tmp/test-session",
                    "status": "unknown_running",
                    "session_meta_status": "idle",
                    "session_meta_last_turn": 14,
                    "session_meta_rounds": 14,
                    "process_tree_active": true,
                    "updated_at_secs": 10
                }]
            }
        });

        enrich_controller_loop_status_with_mcp_state_at(&mut status, &app_state, 12345);

        let wrapper = status.pointer("/active/wrappers/0").unwrap();
        assert_eq!(
            wrapper.get("status").and_then(|value| value.as_str()),
            Some("running_agent")
        );
        assert_eq!(
            wrapper
                .get("session_meta_status")
                .and_then(|value| value.as_str()),
            Some("running_agent")
        );
        assert_eq!(
            wrapper
                .get("raw_session_meta_status")
                .and_then(|value| value.as_str()),
            Some("idle")
        );
        assert_eq!(
            status
                .pointer("/latest/status/state")
                .and_then(|value| value.as_str()),
            Some("running_agent")
        );
    }

    #[test]
    fn controller_loop_status_preserves_idle_app_server_residency_after_round() {
        let mut app_state = McpAppState::new(
            "openai".to_string(),
            "gpt-5.2-codex".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test-session"),
        );
        app_state.link_session_aliases("wrapper-session", "codex-thread");
        app_state.note_session_phase(
            Some("codex-thread"),
            Some(14),
            Phase::WaitingFollowUp,
            Some("Codex follow-up round 14 complete"),
        );
        app_state.note_session_round(Some("codex-thread"), 14);

        let mut status = serde_json::json!({
            "latest": {
                "status": {
                    "run_id": "stale-run",
                    "state": "idle"
                }
            },
            "active": {
                "wrappers": [{
                    "run_id": null,
                    "pid": null,
                    "codex_pid": 8892,
                    "app_server_pid": 8892,
                    "app_server_active": true,
                    "source": "external_wrapper_index",
                    "backend_source": "codex",
                    "backend_session_id": "codex-thread",
                    "intendant_session_id": "wrapper-session",
                    "log_path": "/tmp/test-session",
                    "status": "unknown_running",
                    "session_meta_status": "idle",
                    "process_tree_active": true,
                    "updated_at_secs": 10
                }]
            }
        });

        enrich_controller_loop_status_with_mcp_state_at(&mut status, &app_state, 12345);

        let wrapper = status.pointer("/active/wrappers/0").unwrap();
        assert_eq!(
            wrapper.get("phase").and_then(|value| value.as_str()),
            Some("waiting_follow_up")
        );
        assert_eq!(
            wrapper.get("status").and_then(|value| value.as_str()),
            Some("unknown_running")
        );
        assert_eq!(
            wrapper
                .get("session_meta_status")
                .and_then(|value| value.as_str()),
            Some("idle")
        );
        assert!(wrapper.get("raw_session_meta_status").is_none());
        assert_eq!(
            wrapper
                .get("updated_at_secs")
                .and_then(|value| value.as_u64()),
            Some(10)
        );
        assert_eq!(
            status
                .pointer("/latest/status/state")
                .and_then(|value| value.as_str()),
            Some("waiting_follow_up")
        );
        assert_eq!(
            status
                .pointer("/latest/status/phase")
                .and_then(|value| value.as_str()),
            Some("waiting_follow_up")
        );
        assert_eq!(
            status
                .pointer("/latest/status/turn")
                .and_then(|value| value.as_u64()),
            Some(14)
        );
        assert!(controller_loop_intervention_markers_are_stale(
            false,
            false,
            &[wrapper.clone()],
            &[serde_json::json!({"pid": 8892})]
        ));
    }

    #[test]
    fn controller_loop_status_reports_live_interrupted_phase_over_app_server_residency() {
        let mut app_state = McpAppState::new(
            "openai".to_string(),
            "gpt-5.2-codex".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            std::path::PathBuf::from("/tmp/test-session"),
        );
        app_state.link_session_aliases("wrapper-session", "codex-thread");
        app_state.note_session_phase(
            Some("codex-thread"),
            Some(14),
            Phase::Interrupted,
            Some("Codex follow-up round 14 interrupted"),
        );
        app_state.note_session_round(Some("codex-thread"), 14);

        let mut status = serde_json::json!({
            "latest": {
                "status": {
                    "run_id": "stale-run",
                    "state": "running"
                }
            },
            "active": {
                "wrappers": [{
                    "run_id": null,
                    "pid": null,
                    "codex_pid": 8892,
                    "app_server_pid": 8892,
                    "app_server_active": true,
                    "source": "external_wrapper_index",
                    "backend_source": "codex",
                    "backend_session_id": "codex-thread",
                    "intendant_session_id": "wrapper-session",
                    "log_path": "/tmp/test-session",
                    "status": "unknown_running",
                    "session_meta_status": "idle",
                    "process_tree_active": true,
                    "updated_at_secs": 10
                }]
            }
        });

        enrich_controller_loop_status_with_mcp_state_at(&mut status, &app_state, 12345);

        let wrapper = status.pointer("/active/wrappers/0").unwrap();
        assert_eq!(
            wrapper.get("phase").and_then(|value| value.as_str()),
            Some("interrupted")
        );
        assert_eq!(
            wrapper.get("status").and_then(|value| value.as_str()),
            Some("unknown_running")
        );
        assert_eq!(
            status
                .pointer("/latest/status/state")
                .and_then(|value| value.as_str()),
            Some("interrupted")
        );
        assert_eq!(
            status
                .pointer("/latest/status/turn")
                .and_then(|value| value.as_u64()),
            Some(14)
        );
    }

    #[test]
    fn controller_loop_status_uses_indexed_app_server_when_wrapper_pid_is_alive() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let loop_dir = home.join(".intendant/controller-loop");
        let log_dir = home.join(".intendant/logs/wrapper-session");
        std::fs::create_dir_all(&loop_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrapper-session",
                "created_at": "2026-01-01T00:00:00Z",
                "status": "idle"
            })
            .to_string(),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            home,
            "codex",
            "724fafac-36d7-41e5-b822-e0a08c1f4701",
            "wrapper-session",
            &log_dir,
            None,
        )
        .unwrap();

        let mut wrappers = vec![serde_json::json!({
            "run_id": "20260101T000000Z-1297050",
            "pid": 1297050
        })];
        wrappers.extend(active_external_wrappers_from_index_homes_with_probe(
            [home.to_path_buf()].iter(),
            &[1298123],
            |pid| pid == 1298123,
        ));

        let latest = controller_loop_latest_status(
            serde_json::json!({
                "run_id": "20260101T000000Z-1297050",
                "state": "idle",
                "process_tree_active": false
            }),
            &wrappers,
        );
        assert_eq!(
            latest.get("state").and_then(|value| value.as_str()),
            Some("unknown_running")
        );
        assert_eq!(
            latest
                .get("app_server_pid")
                .and_then(|value| value.as_u64()),
            Some(1298123)
        );
        assert_eq!(
            latest
                .get("app_server_active")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        assert_eq!(
            latest
                .get("session_meta_status")
                .and_then(|value| value.as_str()),
            Some("idle")
        );
    }

    #[test]
    fn controller_loop_status_filters_stale_wrapper_index_by_live_mcp_session() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let stale_intendant_session_id = "4470a234-c76e-4dd7-81fe-27b703dab1c4";
        let stale_backend_session_id = "019ea864-3e83-7f73-aff2-4710faaf2b3f";
        let current_session_id = "e9532107-8c7f-4c1f-b88d-410d6d365505";
        let stale_log_dir = home
            .join(".intendant/logs")
            .join(stale_intendant_session_id);
        let current_log_dir = home.join(".intendant/logs").join(current_session_id);
        std::fs::create_dir_all(&stale_log_dir).unwrap();
        std::fs::create_dir_all(&current_log_dir).unwrap();
        std::fs::write(
            stale_log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": stale_intendant_session_id,
                "created_at": "2026-01-01T00:00:00Z",
                "status": "idle"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            current_log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": current_session_id,
                "created_at": "2026-01-01T00:00:00Z",
                "status": "running"
            })
            .to_string(),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            home,
            "codex",
            stale_backend_session_id,
            stale_intendant_session_id,
            &stale_log_dir,
            None,
        )
        .unwrap();

        let live_process = LiveCodexAppServerProcess {
            pid: 8955,
            mcp_session_id: Some(current_session_id.to_string()),
        };
        let wrappers = active_external_wrappers_from_index_homes_for_processes_with_probe(
            [home.to_path_buf()].iter(),
            std::slice::from_ref(&live_process),
            |pid| pid == 8955,
        );
        assert!(
            wrappers.is_empty(),
            "stale wrapper index row must not be paired with a live current-session app-server"
        );

        let mut active_codex =
            live_codex_app_server_processes_from_infos(std::slice::from_ref(&live_process));
        let mut app_state = McpAppState::new(
            "openai".to_string(),
            "gpt-5.2-codex".to_string(),
            autonomy::shared_autonomy(AutonomyState::default()),
            current_log_dir,
        );
        app_state.session_id = current_session_id.to_string();
        app_state.note_session_phase(
            Some(current_session_id),
            Some(21),
            Phase::RunningAgent,
            Some("current Codex turn"),
        );
        app_state.note_session_round(Some(current_session_id), 21);
        enrich_controller_loop_codex_with_mcp_state(&mut active_codex, &app_state, 12345);

        let latest = controller_loop_latest_status_with_codex(
            serde_json::json!({
                "run_id": "stale-run",
                "state": "idle",
                "source": "external_wrapper_index",
                "backend_session_id": stale_backend_session_id,
                "intendant_session_id": stale_intendant_session_id
            }),
            &wrappers,
            &active_codex,
        );
        assert_eq!(
            latest.get("source").and_then(|value| value.as_str()),
            Some("process_tree")
        );
        assert_eq!(
            latest
                .get("intendant_session_id")
                .and_then(|value| value.as_str()),
            Some(current_session_id)
        );
        assert_ne!(
            latest
                .get("intendant_session_id")
                .and_then(|value| value.as_str()),
            Some(stale_intendant_session_id)
        );
        assert_eq!(
            latest.get("state").and_then(|value| value.as_str()),
            Some("running_agent")
        );
        assert_eq!(
            latest.get("turn").and_then(|value| value.as_u64()),
            Some(21)
        );
    }

    #[test]
    fn codex_app_server_process_tree_active_includes_root_pid_liveness() {
        assert!(codex_app_server_process_tree_active_with_root(
            1298123,
            std::iter::empty(),
            |pid| pid == 1298123,
            |_| None,
        ));
    }

    #[test]
    fn codex_app_server_process_tree_active_requires_live_descendant_cmdline_when_root_dead() {
        let cmdlines = std::collections::HashMap::from([
            (101, "cargo build --release".to_string()),
            (102, String::new()),
            (103, "sleep 60".to_string()),
        ]);

        assert!(codex_app_server_process_tree_active_with_root(
            100,
            [101],
            |pid| pid == 101,
            |pid| cmdlines.get(&pid).cloned(),
        ));
        assert!(codex_app_server_process_tree_active_from_descendants(
            [101],
            |_| true,
            |pid| cmdlines.get(&pid).cloned(),
        ));
        assert!(!codex_app_server_process_tree_active_from_descendants(
            [101],
            |_| false,
            |pid| cmdlines.get(&pid).cloned(),
        ));
        assert!(!codex_app_server_process_tree_active_from_descendants(
            [102],
            |_| true,
            |pid| cmdlines.get(&pid).cloned(),
        ));
    }

    #[test]
    fn controller_loop_status_normalizes_stale_wrapper_index_identity_from_log_path() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let loop_dir = home.join(".intendant/controller-loop");
        let log_dir = home.join(".intendant/logs/resumed-wrapper-session");
        std::fs::create_dir_all(&loop_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "resumed-wrapper-session",
                "created_at": "2026-01-01T00:00:00Z",
                "status": "running"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            crate::external_wrapper_index::index_path(home),
            serde_json::json!({
                "version": 1,
                "wrappers": [{
                    "source": "codex",
                    "backend_session_id": "8b008615-9bf6-44a6-9d26-751e4fd7d87f",
                    "intendant_session_id": "5f979c8d-65e7-4210-be22-e4012242b745",
                    "log_path": log_dir,
                    "updated_at_secs": 1
                }]
            })
            .to_string(),
        )
        .unwrap();

        let wrappers = active_external_wrappers_from_index(&loop_dir, home, &[1084559]);
        assert_eq!(wrappers.len(), 1);
        assert_eq!(
            wrappers[0]
                .get("intendant_session_id")
                .and_then(|value| value.as_str()),
            Some("resumed-wrapper-session")
        );
        assert_eq!(
            wrappers[0].get("log_path").and_then(|value| value.as_str()),
            Some(log_dir.to_string_lossy().as_ref())
        );
        let latest = latest_status_from_active_wrappers(&wrappers).unwrap();
        assert_eq!(
            latest
                .get("intendant_session_id")
                .and_then(|value| value.as_str()),
            Some("resumed-wrapper-session")
        );
    }

    #[test]
    fn controller_loop_status_searches_user_home_wrapper_index_for_project_local_loop_dir() {
        let dir = tempdir().unwrap();
        let project_home = dir.path().join("project");
        let user_home = dir.path().join("home");
        let loop_dir = project_home.join(".intendant/controller-loop");
        let project_root = dir.path().join("workspace");
        let log_dir = user_home.join(".intendant/logs/wrapper-session");
        std::fs::create_dir_all(&loop_dir).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrapper-session",
                "created_at": "2026-01-01T00:00:00Z",
                "status": "running",
                "project_root": project_root
            })
            .to_string(),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            &user_home,
            "codex",
            "019e9b9a-8557-7b01-99ef-187e8840327f",
            "wrapper-session",
            &log_dir,
            Some(&project_root),
        )
        .unwrap();

        let candidate_homes = vec![project_home, user_home];
        let wrappers = active_external_wrappers_from_index_homes(candidate_homes.iter(), &[8892]);
        assert_eq!(wrappers.len(), 1);
        assert_eq!(
            wrappers[0]
                .get("backend_session_id")
                .and_then(|value| value.as_str()),
            Some("019e9b9a-8557-7b01-99ef-187e8840327f")
        );
        assert_eq!(
            wrappers[0]
                .get("intendant_session_id")
                .and_then(|value| value.as_str()),
            Some("wrapper-session")
        );
        assert_eq!(
            wrappers[0]
                .get("codex_pid")
                .and_then(|value| value.as_u64()),
            Some(8892)
        );
        let project_root_string = project_root.to_string_lossy().to_string();
        assert_eq!(
            wrappers[0]
                .get("project_root")
                .and_then(|value| value.as_str()),
            Some(project_root_string.as_str())
        );
    }

    #[test]
    fn controller_loop_status_prefers_live_codex_cwd_project_root() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let helper_root = home.join("helper-root");
        let station_root = home.join("station-worktree");
        let log_dir = home.join(".intendant/logs/wrapper-session");
        std::fs::create_dir_all(&helper_root).unwrap();
        std::fs::create_dir_all(station_root.join(".git")).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(
            log_dir.join("session_meta.json"),
            serde_json::json!({
                "session_id": "wrapper-session",
                "created_at": "2026-01-01T00:00:00Z",
                "status": "running",
                "project_root": helper_root
            })
            .to_string(),
        )
        .unwrap();
        crate::external_wrapper_index::upsert(
            home,
            "codex",
            "019ea0a9-92fc-7471-85d8-0a281fc54250",
            "wrapper-session",
            &log_dir,
            Some(&helper_root),
        )
        .unwrap();

        let wrappers = active_external_wrappers_from_index_homes_with_probe_and_cwd(
            [home.to_path_buf()].iter(),
            &[1588453],
            |pid| pid == 1588453,
            |pid| (pid == 1588453).then(|| station_root.clone()),
        );
        assert_eq!(wrappers.len(), 1);
        let station_root_string = station_root.to_string_lossy().to_string();
        assert_eq!(
            wrappers[0].get("cwd").and_then(|value| value.as_str()),
            Some(station_root_string.as_str())
        );
        assert_eq!(
            wrappers[0]
                .get("project_root")
                .and_then(|value| value.as_str()),
            Some(station_root_string.as_str())
        );
    }

    #[test]
    fn controller_loop_status_does_not_overreport_index_wrappers_without_live_pids() {
        let dir = tempdir().unwrap();
        let home = dir.path();
        let loop_dir = home.join(".intendant/controller-loop");
        std::fs::create_dir_all(&loop_dir).unwrap();
        for idx in 0..2 {
            let log_dir = home.join(format!(".intendant/logs/wrapper-session-{idx}"));
            std::fs::create_dir_all(&log_dir).unwrap();
            std::fs::write(
                log_dir.join("session_meta.json"),
                serde_json::json!({
                    "session_id": format!("wrapper-session-{idx}"),
                    "created_at": "2026-01-01T00:00:00Z",
                    "status": "running"
                })
                .to_string(),
            )
            .unwrap();
            crate::external_wrapper_index::upsert(
                home,
                "codex",
                &format!("019e9b9a-8557-7b01-99ef-187e8840327{idx}"),
                &format!("wrapper-session-{idx}"),
                &log_dir,
                None,
            )
            .unwrap();
        }

        let wrappers = active_external_wrappers_from_index(&loop_dir, home, &[1084559]);
        assert_eq!(wrappers.len(), 1);
        assert_eq!(
            wrappers[0]
                .get("codex_pid")
                .and_then(|value| value.as_u64()),
            Some(1084559)
        );
    }

    #[test]
    fn controller_loop_status_recognizes_codex_app_server_cmdlines() {
        assert!(is_codex_app_server_cmdline(
            "/home/user/projects/codex/codex-rs/target/debug/codex --dangerously-bypass-approvals-and-sandbox app-server -c mcp_servers.intendant.url=..."
        ));
        assert!(is_codex_app_server_cmdline(
            "/opt/homebrew/bin/codex app-server -c model_auto_compact_token_limit=9223372036854775807"
        ));
        assert!(!is_codex_app_server_cmdline(
            "/home/user/projects/intendant/target/release/intendant --web 8892"
        ));
        assert!(!is_codex_app_server_cmdline("[codex] <defunct>"));
        assert!(!is_codex_app_server_cmdline(
            "/usr/bin/codex completion bash"
        ));
    }

    #[test]
    fn controller_loop_status_extracts_mcp_session_id_from_codex_app_server_cmdline() {
        assert_eq!(
            codex_app_server_mcp_session_id_from_cmdline(
                "/home/user/projects/codex-minimal-lineage-aae40ce1f/codex-rs/target/debug/codex \
                 --dangerously-bypass-approvals-and-sandbox app-server -c \
                 mcp_servers.intendant.url=\"http://localhost:8955/mcp?session_id=e9532107-8c7f-4c1f-b88d-410d6d365505&managed_context=managed&tool_profile=core\""
            )
            .as_deref(),
            Some("e9532107-8c7f-4c1f-b88d-410d6d365505")
        );
        assert_eq!(
            codex_app_server_mcp_session_id_from_cmdline(
                "/opt/homebrew/bin/codex app-server -c \
                 mcp_servers.intendant.url=\"http://localhost:8765/mcp?session_id=session%20with%20spaces&managed_context=managed\""
            )
            .as_deref(),
            Some("session with spaces")
        );
        assert_eq!(
            codex_app_server_mcp_session_id_from_cmdline(
                "/opt/homebrew/bin/codex app-server -c mcp_servers.intendant.url=http://localhost:8765/mcp?managed_context=managed"
            ),
            None
        );
    }

    #[test]
    fn controller_loop_status_selects_process_tree_codex_app_servers() {
        let known = HashSet::from([22]);
        let cmdlines = std::collections::HashMap::from([
            (
                11,
                "/opt/homebrew/bin/codex app-server -c foo=bar".to_string(),
            ),
            (22, "/home/user/bin/codex app-server".to_string()),
            (33, "/home/user/bin/codex exec --json".to_string()),
            (44, "/bin/sh -c sleep 60".to_string()),
            (55, "/tmp/codex app-server".to_string()),
        ]);

        let pids =
            live_codex_app_server_pids_from_descendants([55, 44, 33, 22, 11, 11], &known, |pid| {
                cmdlines.get(&pid).cloned()
            });

        assert_eq!(pids, vec![11, 55]);
    }

    #[test]
    fn controller_loop_intervention_mode_validation() {
        let dir = tempdir().unwrap();
        let loop_dir = dir.path().join(".intendant/controller-loop");
        let intervention =
            request_loop_intervention_marker_for_root(&loop_dir, "stop", u32::MAX).unwrap();
        assert_eq!(intervention.mode.as_str(), "stop");
        assert!(intervention.signaled_codex_app_server_pids.is_empty());
        assert!(loop_dir.join("request_stop").exists());

        let err =
            request_loop_intervention_marker_for_root(&loop_dir, "bad", u32::MAX).unwrap_err();
        assert!(err.contains("expected 'stop' or 'abort'"));
    }

    #[test]
    fn intervention_order_report_detects_out_of_order_events() {
        let dir = tempdir().unwrap();
        let run_dir = dir.path().join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(
            run_dir.join("intervention.log"),
            "2026-01-01T00:00:00Z run_started run_id=x\n\
             2026-01-01T00:00:01Z cleanup_begin state=exited\n\
             2026-01-01T00:00:02Z codex_started codex_pid=1\n\
             2026-01-01T00:00:03Z cleanup_end state=exited\n",
        )
        .unwrap();
        let report = intervention_order_report(&run_dir);
        assert_eq!(report["has_log"].as_bool(), Some(true));
        assert_eq!(report["order_ok"].as_bool(), Some(false));
    }

    #[tokio::test]
    async fn schedule_restart_normalizes_string_fields() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let output = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "  codex  ".to_string(),
                north_star_goal: "  improve loop  ".to_string(),
                reason: Some("  periodic refresh  ".to_string()),
                restart_after: Some("  NOW  ".to_string()),
                restart_command: Some("  true  ".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(true));

        let s = state.read().await;
        let restart = s
            .controller_restart
            .as_ref()
            .expect("restart should be stored");
        assert_eq!(restart.controller_id, "codex");
        assert_eq!(restart.north_star_goal, "improve loop");
        assert_eq!(restart.reason.as_deref(), Some("periodic refresh"));
        assert_eq!(restart.restart_after, RestartAfter::Now);
        assert_eq!(restart.restart_command.as_deref(), Some("true"));
    }

    #[tokio::test]
    async fn spawn_detached_restart_command_returns_live_pid() {
        // A long-lived command per platform: `sleep` on Unix, a long `timeout`
        // on Windows (the cmd.exe-resolvable form, /T seconds, /NOBREAK so it
        // doesn't consume our detached stdin).
        #[cfg(windows)]
        let long_running = "timeout /T 30 /NOBREAK";
        #[cfg(not(windows))]
        let long_running = "sleep 30";

        let pid = spawn_detached_restart_command(long_running)
            .await
            .expect("detached spawn should succeed");
        assert!(pid > 1);

        // Liveness via the platform helper (kill(pid,0) on Unix,
        // OpenProcess/GetExitCodeProcess on Windows) rather than shelling to
        // bash, which doesn't exist on a stock Windows host.
        assert!(
            crate::platform::process_alive(pid),
            "spawned pid should be alive"
        );

        // Best-effort cleanup of the detached child.
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/F", "/T"])
                .status();
        }
        #[cfg(not(windows))]
        {
            let _ = std::process::Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }

    #[tokio::test]
    async fn controller_turn_complete_marks_restart_failed_when_auto_start_task_fails() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: None,
                auto_start_task: Some(true),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        {
            let mut s = state.write().await;
            s.set_phase(Phase::RunningAgent);
        }

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: restart_id.clone(),
                turn_complete_token: token,
                status: None,
                handoff_summary: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("restart_pending"));
        assert_eq!(json["phase"].as_str(), Some("failed"));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("Failed to start follow-up task"));

        let restart_path = restart_state_path(dir.path());
        let persisted = std::fs::read_to_string(restart_path).expect("restart file should exist");
        let persisted_json: serde_json::Value = serde_json::from_str(&persisted).unwrap();
        let restart_json = persisted_json.as_object().expect("restart should persist");
        assert_eq!(
            restart_json.get("phase").and_then(|v| v.as_str()),
            Some("failed")
        );
        assert!(restart_json
            .get("last_error")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .contains("Failed to start follow-up task"));
    }

    #[tokio::test]
    async fn controller_turn_complete_rejects_ready_phase() {
        let dir = tempdir().unwrap();
        let state = test_state_with_log_dir(dir.path().to_path_buf());
        let bus = EventBus::new();
        let server = IntendantServer::new(state.clone(), bus);

        let scheduled = server
            .schedule_controller_restart(Parameters(ScheduleControllerRestartParams {
                controller_id: "codex".to_string(),
                north_star_goal: "improve loop".to_string(),
                reason: None,
                restart_after: Some("turn_end".to_string()),
                restart_command: Some("true".to_string()),
                auto_start_task: Some(false),
                max_attempts: None,
                cooldown_sec: None,
            }))
            .await;
        let scheduled_json: serde_json::Value = serde_json::from_str(&scheduled).unwrap();
        let restart_id = scheduled_json["restart_id"].as_str().unwrap().to_string();
        let token = scheduled_json["turn_complete_token"]
            .as_str()
            .unwrap()
            .to_string();

        {
            let mut s = state.write().await;
            let restart = s
                .controller_restart
                .as_mut()
                .expect("restart should be tracked");
            restart.phase = RestartPhase::Ready;
        }

        let output = server
            .controller_turn_complete(Parameters(ControllerTurnCompleteParams {
                restart_id: restart_id.clone(),
                turn_complete_token: token,
                status: None,
                handoff_summary: None,
            }))
            .await;
        let json: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(json["ok"].as_bool(), Some(false));
        assert_eq!(json["status"].as_str(), Some("rejected"));
        assert_eq!(json["restart_id"].as_str(), Some(restart_id.as_str()));
        assert_eq!(json["phase"].as_str(), Some("ready"));
        assert!(json["error"]
            .as_str()
            .unwrap_or("")
            .contains("Restart is not awaiting completion"));
    }
}
