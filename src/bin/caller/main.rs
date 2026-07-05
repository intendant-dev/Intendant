mod access;
mod agent_runner;
mod app_state_pricing;
mod approval;
mod atspi_read;
mod audio_routing;
mod autonomy;
#[cfg(target_os = "macos")]
mod ax;
mod browser_workspace;
mod computer_use;
mod connect_rendezvous;
mod context_rewind;
mod control;
mod control_plane;
mod conversation;
mod credential_audit;
mod credential_egress;
mod credential_leases;
mod ctl;
mod daemon_identity;
mod daemon_log_tee;
mod dashboard_control;
mod debug;
mod diagnostics;
mod display;
mod error;
mod event;
mod external_agent;
mod external_wrapper_index;
mod file_watcher;
mod fission_ledger;
mod fission_lifecycle;
mod frames;
mod frontend;
mod gateway_routes;
mod knowledge;
mod lineage_ledger;
mod linux_display_env;
mod live_audio;
mod live_audio_types;
mod mcp;
mod mcp_client;
mod peer;
mod peer_file_transfer;
mod platform;
mod presence;
mod project;
mod prompts;
mod provider;
mod provider_mock;
mod quarantine;
mod recording;
mod sandbox;
mod schema_validator;
mod service_mode;
mod session_config;
mod session_identity;
mod session_log;
mod session_names;
mod session_supervisor;
mod setup;
mod skills;
mod sub_agent;
mod task_dispatch;
mod terminal;
mod tool_batch;
mod tools;
mod transcription;
mod transfer_store;
mod tui;
mod types;
mod upload_store;
mod vision;
mod web_gateway;
mod web_tls;
#[cfg(windows)]
#[path = "../../win_sandbox.rs"]
mod win_sandbox;
mod windows_uia;
mod worktree;
mod worktree_inventory;
#[cfg(target_os = "linux")]
mod x11_input;

// God-file split (see CLAUDE.md "File size budget"): regions extracted from
// this file live in the modules below; the glob re-exports keep existing
// crate:: and unqualified references resolving unchanged.
mod codex_history;
pub(crate) use codex_history::*;
mod external_output;
pub(crate) use external_output::*;
mod steering;
pub(crate) use steering::*;
mod managed_context_ops;
pub(crate) use managed_context_ops::*;
mod thread_actions;
pub(crate) use thread_actions::*;
mod external_events;
pub(crate) use external_events::*;
mod startup;
pub(crate) use startup::*;
mod agent_loop;
pub(crate) use agent_loop::*;
mod run_modes;
pub(crate) use run_modes::*;
mod external_mode;
pub(crate) use external_mode::*;

use autonomy::{AutonomyLevel, AutonomyState, SharedAutonomy};
use conversation::Conversation;
use error::CallerError;
use event::{AppEvent, EventBus};
use project::Project;
use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tool_batch::{assemble_batch_from_tool_calls, map_results_to_tool_responses};

type SharedSessionLog = Arc<Mutex<session_log::SessionLog>>;

/// Session log directory for the panic hook to write panic.log into.
/// Set once when a session starts; read by the panic hook on crash.
static PANIC_LOG_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Shared slot for JSON-mode approval responses.
/// The stdin reader stores approval senders here; the agent loop awaits them.
type JsonApprovalSlot =
    Arc<Mutex<Option<(u64, tokio::sync::oneshot::Sender<event::ApprovalResponse>)>>>;

fn new_json_approval_slot() -> JsonApprovalSlot {
    Arc::new(Mutex::new(None))
}

/// Helper to write to the session log without propagating errors.
fn slog(log: &SharedSessionLog, f: impl FnOnce(&mut session_log::SessionLog)) {
    if let Ok(mut l) = log.lock() {
        f(&mut l);
    }
}

fn session_log_id(session_log: &SharedSessionLog) -> Option<String> {
    session_log
        .lock()
        .ok()
        .map(|log| log.session_id().to_string())
        .filter(|id| !id.is_empty())
}

fn external_resume_session_for_startup(
    backend: Option<&external_agent::AgentBackend>,
    flags: &CliFlags,
    intendant_session_id: Option<&str>,
) -> Option<String> {
    external_resume_session_for_startup_in_home(
        &platform::home_dir(),
        backend,
        flags,
        intendant_session_id,
    )
}

fn external_resume_session_for_startup_in_home(
    home: &Path,
    backend: Option<&external_agent::AgentBackend>,
    flags: &CliFlags,
    intendant_session_id: Option<&str>,
) -> Option<String> {
    let backend = backend?;
    let intendant_session_id = intendant_session_id
        .map(str::trim)
        .filter(|id| !id.is_empty())?;
    let requested_resume_token = flags
        .resume_id
        .as_deref()
        .or(flags.continue_last.then_some(intendant_session_id))?;
    let token = session_supervisor::effective_external_resume_token_in_home(
        home,
        backend.as_short_str(),
        intendant_session_id,
        requested_resume_token,
        false,
    );
    (!token.trim().is_empty()).then_some(token)
}

/// Rehydrate the persisted per-session agent config for a CLI startup resume
/// (`--resume` / `--continue` with an external backend) and lay it over
/// `project`, mirroring the precedence `SessionSupervisor::resume_session`
/// applies on the daemon path:
///
///   explicit overrides > persisted per-session config > global/TOML project
///
/// Returns the effective per-session overrides so callers can forward the
/// fields that don't live in the project (`codex_service_tier`,
/// `codex_home`) to the agent, or `None` when there is nothing to apply
/// (fresh startup, no resume token, or no persisted config).
fn apply_startup_external_resume_config(
    backend: &external_agent::AgentBackend,
    project: &mut Project,
    intendant_session_id: Option<&str>,
    resume_session: Option<&str>,
) -> Option<session_config::SessionAgentConfig> {
    apply_startup_external_resume_config_in_home(
        &platform::home_dir(),
        backend,
        project,
        intendant_session_id,
        resume_session,
        // No per-field agent CLI flags exist today (only `--agent <BACKEND>`),
        // so there are no explicit overrides to protect at startup. If such
        // flags are added, build this from them (see `session_config::from_wire`)
        // so they keep winning over the persisted per-session config.
        session_config::SessionAgentConfig::default(),
    )
}

fn apply_startup_external_resume_config_in_home(
    home: &Path,
    backend: &external_agent::AgentBackend,
    project: &mut Project,
    intendant_session_id: Option<&str>,
    resume_session: Option<&str>,
    explicit_overrides: session_config::SessionAgentConfig,
) -> Option<session_config::SessionAgentConfig> {
    let resume_token = resume_session
        .map(str::trim)
        .filter(|token| !token.is_empty())?;
    let session_id = intendant_session_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .unwrap_or(resume_token);
    let mut config = explicit_overrides;
    if let Some(persisted) = session_config::load_for_resume(
        home,
        backend.as_short_str(),
        session_id,
        Some(resume_token),
    ) {
        config.merge_missing_from(persisted);
    }
    if config.is_empty() {
        return None;
    }
    session_config::apply_to_project(project, backend, &config);
    Some(config)
}

fn emit_external_session_identity(
    bus: &EventBus,
    session_id: Option<String>,
    source: &str,
    backend_session_id: &str,
) {
    let Some(session_id) = session_id.filter(|id| !id.trim().is_empty()) else {
        return;
    };
    bus.send(AppEvent::SessionIdentity {
        session_id,
        source: source.to_string(),
        backend_session_id: backend_session_id.to_string(),
    });
}

fn record_external_done_and_round_inline(
    session_log: &SharedSessionLog,
    enabled: bool,
    session_id: Option<&str>,
    message: Option<&str>,
    round: usize,
    turns_in_round: usize,
) {
    if !enabled {
        return;
    }
    slog(session_log, |log| {
        log.done_signal_for_session(session_id, message);
        log.round_complete(round, turns_in_round);
    });
}

fn record_external_round_inline(
    session_log: &SharedSessionLog,
    enabled: bool,
    round: usize,
    turns_in_round: usize,
) {
    if !enabled {
        return;
    }
    slog(session_log, |log| log.round_complete(round, turns_in_round));
}

fn external_rollback_turn_in_progress(err: &CallerError) -> bool {
    let CallerError::ExternalAgent(message) = err else {
        return false;
    };
    message
        .to_ascii_lowercase()
        .contains("cannot rollback while a turn is in progress")
}

fn event_targets_session(target: &Option<String>, session_id: &Option<String>) -> bool {
    match target {
        Some(target) => session_id.as_deref() == Some(target.as_str()),
        None => true,
    }
}

fn event_targets_session_or_alias(
    target: &Option<String>,
    session_id: &Option<String>,
    alias_session_id: &Option<String>,
) -> bool {
    match target {
        Some(target) => {
            session_id.as_deref() == Some(target.as_str())
                || alias_session_id.as_deref() == Some(target.as_str())
        }
        None => true,
    }
}

/// Rotate the CLI external-agent loop's primary address to a newly announced
/// native session id: the native id becomes `session_id` (what results and
/// scoped events carry) and the previous primary — the Intendant log id —
/// stays reachable as the alias, so targeted controls match under either
/// name. Without this, a backend that starts on a placeholder id (Claude
/// Code) could never receive thread actions addressed to its upgraded id.
fn rotate_external_identity(
    native_id: &str,
    live_session_id: &mut Option<String>,
    drain_config: &mut DrainConfig<'_>,
) {
    let native_id = native_id.trim();
    if native_id.is_empty() || live_session_id.as_deref() == Some(native_id) {
        return;
    }
    drain_config.alias_session_id = live_session_id
        .clone()
        .or_else(|| drain_config.alias_session_id.clone());
    *live_session_id = Some(native_id.to_string());
    drain_config.session_id = live_session_id.clone();
    drain_config.backend_thread_id = Some(native_id.to_string());
}

fn event_targets_external_session_or_side(
    target: &Option<String>,
    session_id: &Option<String>,
    alias_session_id: &Option<String>,
    side_threads: &HashMap<String, String>,
) -> bool {
    match target {
        Some(target) => {
            event_targets_session_or_alias(&Some(target.clone()), session_id, alias_session_id)
                || side_threads.contains_key(target)
        }
        None => true,
    }
}

fn event_targets_external_session_or_optional_side(
    target: &Option<String>,
    session_id: &Option<String>,
    alias_session_id: &Option<String>,
    side_threads: Option<&HashMap<String, String>>,
) -> bool {
    match side_threads {
        Some(side_threads) => event_targets_external_session_or_side(
            target,
            session_id,
            alias_session_id,
            side_threads,
        ),
        None => event_targets_session_or_alias(target, session_id, alias_session_id),
    }
}

/// Emit a "[runtime] Task dispatched" log entry from a backend task acceptance
/// point. Writes to the session log on disk and broadcasts a `LogEntry` event
/// for external consumers (web dashboard, control socket).
///
/// This is the single source of truth for the dispatch log line: it lives in
/// the backend (where the task is actually accepted for processing) rather
/// than in any frontend, so the log is consistent across TUI, headless, and
/// daemon modes regardless of which interface originated the task.
fn emit_task_dispatched_log(
    bus: &EventBus,
    session_log: &SharedSessionLog,
    task: &str,
    attachment_count: usize,
) {
    let suffix = if attachment_count > 0 {
        format!(
            " with {} attachment{}",
            attachment_count,
            if attachment_count == 1 { "" } else { "s" }
        )
    } else {
        String::new()
    };
    let message = format!(
        "[runtime] Task dispatched{}: {}",
        suffix,
        types::truncate_str(task, 80)
    );
    slog(session_log, |l| l.info(&message));
    bus.send(AppEvent::LogEntry {
        session_id: None,
        level: "info".to_string(),
        source: "system".to_string(),
        content: message,
        turn: None,
    });
}

fn emit_user_message_log(
    bus: &EventBus,
    session_log: &SharedSessionLog,
    session_id: Option<&str>,
    user_turn_index: Option<u32>,
    user_turn_revision: Option<u32>,
    replacement_for_user_turn_index: Option<u32>,
    text: &str,
) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    slog(session_log, |l| l.info(&format!("[user] {}", text)));
    bus.send(AppEvent::UserMessageLog {
        session_id: session_id.map(str::to_string),
        content: text.to_string(),
        user_turn_index,
        user_turn_revision,
        replacement_for_user_turn_index,
    });
}

fn emit_external_session_loop_error(
    bus: &EventBus,
    session_log: &SharedSessionLog,
    session_id: Option<&str>,
    source: &str,
    message: String,
) {
    slog(session_log, |l| l.warn(&message));
    bus.send(AppEvent::LogEntry {
        session_id: session_id.map(str::to_string),
        level: "warn".to_string(),
        source: source.to_string(),
        content: message.clone(),
        turn: None,
    });
    bus.send(AppEvent::LoopError(message));
}

fn json_string_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}

/// Resolve external agent backend from an explicit override, falling back to
/// the project config's `agent.default_backend` setting.
fn resolve_agent_backend_from_config(
    explicit: Option<external_agent::AgentBackend>,
    project: &Project,
) -> Option<external_agent::AgentBackend> {
    explicit.or_else(|| {
        project
            .config
            .agent
            .default_backend
            .as_ref()
            .and_then(|s| external_agent::AgentBackend::from_str_loose(s))
    })
}

/// Structural equality for `CodexRuntimeConfig`. The struct itself doesn't
/// derive `PartialEq` because it's a public API surface and we don't want to
/// commit to field-by-field equality semantics for external callers; inside
/// the daemon loop we just need to detect drift across tasks, so we compare
/// the Codex-locked fields explicitly. Any change here that affects the
/// spawned Codex thread (sandbox, approvals, model, reasoning effort, tool
/// set, sandbox permissions) has to force a rebuild because Codex latches
/// those at `thread/start`.
fn codex_runtime_config_equal(
    a: &control_plane::CodexRuntimeConfig,
    b: &control_plane::CodexRuntimeConfig,
) -> bool {
    a.command == b.command
        && a.managed_command == b.managed_command
        && a.sandbox == b.sandbox
        && a.approval_policy == b.approval_policy
        && a.model == b.model
        && a.reasoning_effort == b.reasoning_effort
        && a.service_tier == b.service_tier
        && a.web_search == b.web_search
        && a.network_access == b.network_access
        && a.writable_roots == b.writable_roots
        && a.managed_context == b.managed_context
        && a.context_archive == b.context_archive
}

fn claude_runtime_config_equal(
    a: &control_plane::ClaudeRuntimeConfig,
    b: &control_plane::ClaudeRuntimeConfig,
) -> bool {
    a.model == b.model
        && a.permission_mode == b.permission_mode
        && a.allowed_tools == b.allowed_tools
}

fn normalize_diff_file_path(path: &str) -> Option<String> {
    let path = path.split('\t').next().unwrap_or(path).trim();
    if path == "/dev/null" {
        return None;
    }
    // Strip exactly one git-style `a/` or `b/` prefix. Codex sometimes
    // produces `b//home/...` (double slash) for absolute paths; that
    // becomes `/home/...` after the single-prefix strip.
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    if path.is_empty() {
        return None;
    }
    Some(path.to_string())
}

/// Extract file paths from a unified-diff header. Reads `+++ b/<path>` lines
/// (git-style), with `--- a/<path>` used as a fallback for pure-delete diffs
/// where the `+++` side is `/dev/null`. Deduplicates while preserving order.
///
/// Used when the external agent's own `files_changed` list is empty, which
/// has been observed for Codex's `turn/diff/updated` notifications in
/// practice — the wire protocol carries the paths only inside the diff body.
fn parse_diff_file_paths(unified_diff: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in unified_diff.lines() {
        let path = if let Some(rest) = line.strip_prefix("+++ ") {
            rest
        } else if let Some(rest) = line.strip_prefix("--- ") {
            rest
        } else {
            continue;
        };
        if let Some(path) = normalize_diff_file_path(path) {
            if !out.iter().any(|p| p == &path) {
                out.push(path);
            }
        }
    }
    out
}

fn diff_line_text(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn is_unified_file_boundary(lines: &[&str], idx: usize) -> bool {
    let line = diff_line_text(lines[idx]);
    line.starts_with("diff --git ")
        || (line.starts_with("--- ")
            && lines
                .get(idx + 1)
                .is_some_and(|next| diff_line_text(next).starts_with("+++ ")))
}

fn split_unified_diff_by_file(unified_diff: &str) -> Vec<(String, String)> {
    if unified_diff.trim().is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<&str> = unified_diff.split_inclusive('\n').collect();
    if lines.is_empty() {
        lines.push(unified_diff);
    }

    let mut starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(idx, line)| {
            diff_line_text(line)
                .starts_with("diff --git ")
                .then_some(idx)
        })
        .collect();
    if starts.is_empty() {
        for idx in 0..lines.len() {
            if is_unified_file_boundary(&lines, idx) {
                starts.push(idx);
            }
        }
    }
    if starts.is_empty() {
        let files = parse_diff_file_paths(unified_diff);
        return files
            .into_iter()
            .next()
            .map(|path| vec![(path, unified_diff.to_string())])
            .unwrap_or_default();
    }

    let mut out = Vec::new();
    for (i, start) in starts.iter().copied().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(lines.len());
        let block = lines[start..end].concat();
        if let Some(path) = parse_diff_file_paths(&block).into_iter().next() {
            out.push((path, block));
        }
    }
    out
}

fn external_diff_log_body(message: &str) -> Option<&str> {
    if !message.starts_with("External agent diff") {
        return None;
    }
    let first_line_end = message.find('\n')?;
    let body = &message[first_line_end + 1..];
    if body.contains("diff --git ") || body.contains("--- ") || body.contains("@@ ") {
        Some(body)
    } else {
        None
    }
}

fn parse_session_diff_file_paths(log_dir: &Path) -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(log_dir.join("session.jsonl")) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for line in contents.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(message) = value.get("message").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(diff_body) = external_diff_log_body(message) else {
            continue;
        };
        for path in parse_diff_file_paths(diff_body) {
            if !out.iter().any(|p| p == &path) {
                out.push(path);
            }
        }
    }
    out
}

fn resolve_diff_file_path(project_root: &Path, display_path: &str) -> Option<PathBuf> {
    let path = Path::new(display_path);
    if path.is_absolute() {
        let allowed = path.starts_with(project_root)
            || path.starts_with(std::env::temp_dir())
            || (cfg!(unix) && (path.starts_with("/tmp") || path.starts_with("/private/tmp")));
        return allowed.then(|| path.to_path_buf());
    }

    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        )
    }) {
        return None;
    }

    Some(project_root.join(path))
}

fn read_diff_file_text(project_root: &Path, display_path: &str) -> Option<Option<String>> {
    let path = resolve_diff_file_path(project_root, display_path)?;
    match std::fs::read_to_string(path) {
        Ok(text) => Some(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    }
}

struct ExternalDiffDelta {
    files_changed: Vec<String>,
    unified_diff: String,
}

#[derive(Default)]
struct ExternalDiffDeltaTracker {
    snapshots: HashMap<String, Option<String>>,
}

impl ExternalDiffDeltaTracker {
    fn seed_current_paths<'a>(
        &mut self,
        project_root: &Path,
        paths: impl IntoIterator<Item = &'a str>,
    ) {
        for path in paths {
            let Some(path) = normalize_diff_file_path(path) else {
                continue;
            };
            let Some(current) = read_diff_file_text(project_root, &path) else {
                continue;
            };
            self.snapshots.insert(path, current);
        }
    }

    fn seed_from_session_log(&mut self, project_root: &Path, log_dir: &Path) {
        let paths = parse_session_diff_file_paths(log_dir);
        self.seed_current_paths(project_root, paths.iter().map(String::as_str));
    }

    fn delta(
        &mut self,
        project_root: &Path,
        files_changed: &[String],
        unified_diff: &str,
    ) -> Option<ExternalDiffDelta> {
        let mut ordered_paths = Vec::new();
        let mut seen = HashSet::new();
        let mut block_by_path = HashMap::new();

        for (path, block) in split_unified_diff_by_file(unified_diff) {
            if seen.insert(path.clone()) {
                ordered_paths.push(path.clone());
            }
            block_by_path.entry(path).or_insert(block);
        }

        for path in files_changed {
            if let Some(path) = normalize_diff_file_path(path) {
                if seen.insert(path.clone()) {
                    ordered_paths.push(path);
                }
            }
        }

        let mut previously_tracked: Vec<String> = self.snapshots.keys().cloned().collect();
        previously_tracked.sort();
        for path in previously_tracked {
            if seen.insert(path.clone()) {
                ordered_paths.push(path);
            }
        }

        let mut delta_diff = String::new();
        let mut delta_files = Vec::new();

        for path in ordered_paths {
            let current = read_diff_file_text(project_root, &path).flatten();
            let maybe_delta = if let Some(previous) = self.snapshots.get(&path) {
                if previous == &current {
                    None
                } else {
                    Some(file_watcher::compute_unified_diff(
                        previous.as_deref().unwrap_or(""),
                        current.as_deref().unwrap_or(""),
                        &path,
                    ))
                }
            } else if let Some(block) = block_by_path.get(&path) {
                Some(block.clone())
            } else {
                current
                    .as_ref()
                    .map(|text| file_watcher::compute_unified_diff("", text, &path))
            };

            self.snapshots.insert(path.clone(), current);

            let Some(file_delta) = maybe_delta else {
                continue;
            };
            if file_delta.trim().is_empty() {
                continue;
            }
            delta_files.push(path);
            delta_diff.push_str(&file_delta);
            if !delta_diff.ends_with('\n') {
                delta_diff.push('\n');
            }
        }

        if delta_diff.trim().is_empty() {
            None
        } else {
            Some(ExternalDiffDelta {
                files_changed: delta_files,
                unified_diff: delta_diff,
            })
        }
    }
}

/// Resolve external agent backend from shared state (written by the web UI),
/// falling back to the project config default.
async fn resolve_agent_backend(
    shared: &Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>>,
    project: &Project,
) -> Option<external_agent::AgentBackend> {
    resolve_agent_backend_from_config(shared.read().await.clone(), project)
}

fn codex_context_trace_dir(
    session_log: &SharedSessionLog,
    context_archive: &str,
) -> (Option<PathBuf>, bool) {
    match project::normalize_codex_context_archive(context_archive).as_str() {
        "off" => (None, false),
        "exact" => (
            session_log
                .lock()
                .ok()
                .map(|log| log.dir().join("model-request-traces")),
            false,
        ),
        _ => {
            let session = session_log_id(session_log)
                .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
            let dir = std::env::temp_dir()
                .join("intendant-context-traces")
                .join(format!("{session}-{}", uuid::Uuid::new_v4().simple()));
            (Some(dir), true)
        }
    }
}

/// Construct, initialize, and start a thread for an external agent backend.
///
/// Returns the agent, thread handle, and event receiver. The caller owns the
/// agent lifetime and is responsible for sending messages and draining events.
async fn create_external_agent(
    backend: &external_agent::AgentBackend,
    project: &Project,
    session_log: &SharedSessionLog,
    web_port: Option<u16>,
    resume_session: Option<String>,
    mcp_session_id: Option<String>,
    codex_service_tier: Option<String>,
    codex_home: Option<String>,
) -> Result<
    (
        Box<dyn external_agent::ExternalAgent>,
        external_agent::AgentThread,
        tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>,
    ),
    CallerError,
> {
    use external_agent::{AgentBackend, AgentConfig};

    let mcp_session_id = mcp_session_id.or_else(|| session_log_id(session_log));
    let mcp_auth_token =
        web_port.map(|_| crate::web_gateway::loopback_mcp_auth_token().to_string());
    // A spawn is the INITIAL fork of another thread exactly while the wrapper
    // still resumes the parent id recorded as `forked_from`; once the child's
    // own native id is persisted, resume moves to the child id and the same
    // wrapper becomes a plain resume.
    let fork_resume = resume_session
        .as_deref()
        .map(str::trim)
        .is_some_and(|resume| {
            session_log
                .lock()
                .ok()
                .map(|log| log.dir().to_path_buf())
                .and_then(|dir| crate::session_config::read_log_dir_config(&dir))
                .and_then(|cfg| cfg.forked_from)
                .is_some_and(|parent| parent.trim() == resume)
        });

    let (mut agent, config): (Box<dyn external_agent::ExternalAgent>, AgentConfig) = match backend {
        AgentBackend::Codex => {
            let cfg = &project.config.agent.codex;
            let sandbox_mode = project::normalize_sandbox_mode(&cfg.sandbox);
            let reasoning_effort =
                project::normalize_reasoning_effort(cfg.reasoning_effort.as_deref());
            let codex_managed_context =
                project::codex_managed_context_enabled(&cfg.managed_context);
            let context_archive = project::normalize_codex_context_archive(&cfg.context_archive);
            let (request_trace_dir, request_trace_temporary) =
                codex_context_trace_dir(session_log, &context_archive);
            let codex_home = codex_home
                .as_deref()
                .and_then(|home| crate::session_config::normalize_codex_home(Some(home)))
                .or_else(crate::session_config::effective_codex_home)
                .map(PathBuf::from);
            let opts = external_agent::codex::CodexAgentOptions {
                reasoning_effort: reasoning_effort.clone(),
                web_search: cfg.web_search,
                network_access: cfg.network_access,
                writable_roots: cfg.writable_roots.clone(),
                managed_context: codex_managed_context,
            };
            // Managed sessions spawn the Intendant-aware fork when one is
            // configured (`codex.managed_command`); vanilla sessions and
            // legacy configs use `codex.command`.
            let agent = Box::new(external_agent::codex::CodexAgent::with_options(
                cfg.effective_command(codex_managed_context),
                cfg.model.clone(),
                cfg.approval_policy.clone(),
                sandbox_mode.clone(),
                web_port,
                opts,
            ));
            let config = AgentConfig {
                model: cfg.model.clone(),
                working_dir: project.root.clone(),
                request_trace_dir,
                request_trace_temporary,
                context_archive,
                approval_policy: cfg.approval_policy.clone(),
                sandbox: sandbox_mode,
                reasoning_effort,
                service_tier: codex_service_tier
                    .or_else(|| project::normalize_codex_service_tier(cfg.service_tier.as_deref())),
                web_search: cfg.web_search,
                network_access: cfg.network_access,
                writable_roots: cfg.writable_roots.clone(),
                codex_managed_context,
                web_port,
                mcp_auth_token: mcp_auth_token.clone(),
                mcp_session_id: mcp_session_id.clone(),
                resume_session: resume_session.clone(),
                fork_resume,
                codex_home,
            };
            (agent, config)
        }
        AgentBackend::ClaudeCode => {
            let cfg = &project.config.agent.claude_code;
            let agent = Box::new(external_agent::claude_code::ClaudeCodeAgent::new(
                cfg.command.clone(),
                cfg.model.clone(),
                cfg.permission_mode.clone(),
                cfg.effort.clone(),
                cfg.allowed_tools.clone(),
                web_port,
            ));
            let config = AgentConfig {
                model: cfg.model.clone(),
                working_dir: project.root.clone(),
                request_trace_dir: None,
                request_trace_temporary: false,
                context_archive: "off".to_string(),
                approval_policy: cfg.permission_mode.clone(),
                sandbox: String::new(),
                reasoning_effort: None,
                service_tier: None,
                web_search: false,
                network_access: false,
                writable_roots: Vec::new(),
                codex_managed_context: false,
                web_port,
                mcp_auth_token: mcp_auth_token.clone(),
                mcp_session_id: mcp_session_id.clone(),
                resume_session: resume_session.clone(),
                fork_resume,
                codex_home: None,
            };
            (agent, config)
        }
    };

    let event_rx = agent.initialize(config).await?;
    slog(session_log, |l| l.debug("External agent initialized"));

    let thread = agent.start_thread().await?;
    slog(session_log, |l| {
        l.debug(&format!("External agent thread: {}", thread.thread_id))
    });

    Ok((agent, thread, event_rx))
}

/// Configuration for `drain_external_agent_events`.
struct DrainConfig<'a> {
    bus: &'a EventBus,
    session_id: Option<String>,
    alias_session_id: Option<String>,
    /// The backend (Codex) thread id of THIS conversation, when the caller
    /// holds the live `AgentThread`. Conversations are named inconsistently
    /// across paths — the CLI external-agent loop uses `session_id` = thread
    /// id with the Intendant session id as the alias, while the daemon's
    /// persistent dispatch loop uses `session_id` = Intendant session id with
    /// the thread id as the alias — so a thread action that targets this
    /// conversation by either name resolves its `threadId` from this field
    /// rather than guessing which of the two ids the backend understands.
    backend_thread_id: Option<String>,
    autonomy: SharedAutonomy,
    session_log: &'a SharedSessionLog,
    project_root: &'a Path,
    log_dir: &'a Path,
    approval_registry: &'a event::ApprovalRegistry,
    json_approval: Option<&'a JsonApprovalSlot>,
    /// Web dashboard port when serving (`--web`). `Some` means an interactive
    /// frontend exists, so external-agent approval requests are surfaced to
    /// the gate rather than auto-denied as if truly headless.
    web_port: Option<u16>,
    agent_source: Option<String>,
    /// When true, `ToolStarted` just increments the turn counter without
    /// emitting `AgentStarted`. The presence path sets this to avoid
    /// duplicating the model reasoning that's already shown via ModelResponse.
    suppress_agent_started: bool,
    /// Supervised external sessions have their own session log in addition to
    /// the daemon's root log writer. Persist model responses here too so
    /// per-session replay does not depend on the root session log.
    persist_model_responses_inline: bool,
    /// When true and no `json_approval` slot is set, auto-deny approval
    /// requests (headless mode with no interactive input).
    headless: bool,
    /// Shared context-injection queue. Fallback target when the backend
    /// does not support mid-turn steering — queued items are drained on
    /// the next turn's follow-up message path.
    context_injection: &'a event::ContextInjectionQueue,
}

const EXTERNAL_CONTEXT_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
struct ExternalContextSnapshotState {
    emitted_keys: std::collections::HashSet<u64>,
    last_error: Option<String>,
}

/// Result of draining one batch of external agent events.
enum DrainOutcome {
    /// The agent's turn completed. The caller decides how to continue
    /// (e.g., wait for follow-up, emit DoneSignal, break inner loop).
    TurnCompleted {
        message: Option<String>,
        turns_in_round: usize,
    },
    /// The agent process terminated.
    Terminated {
        reason: String,
        exit_code: Option<i32>,
    },
    /// The event channel was closed unexpectedly.
    ChannelClosed,
    /// The backend finished a turn in a recoverable error state. The external
    /// agent process is still usable, but the caller must not immediately
    /// submit another ordinary continuation.
    RecoveryRequired {
        message: String,
        recovery_hint: Option<String>,
        turns_in_round: usize,
    },
    /// A user-requested interrupt completed cleanly. The agent was asked to
    /// cancel its turn (e.g. via `session/cancel` or `turn/interrupt`) and
    /// acknowledged with a terminal event. The caller should break its
    /// outer loop the same way it would for `TurnCompleted`, but MUST NOT
    /// wait for a follow-up message — the interrupt *is* the follow-up.
    Interrupted { reason: String },
    /// A model/tool requested context rewind during the active turn. The drain
    /// waits until the backend reports the turn complete, then returns this so
    /// the caller can apply the rollback while the thread is idle.
    ContextRewindRequested {
        request: ExternalContextRewindRequest,
        message: Option<String>,
        turns_in_round: usize,
        turn_stop_status: ManagedContextRewindTurnStopStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExternalBackendRecovery {
    message: String,
    recovery_hint: Option<String>,
}

/// Build the control plane's live Claude Code runtime config from the
/// project TOML. Mirrors the inline Codex/Gemini seeding blocks.
fn shared_claude_config_from_project(project: &Project) -> control_plane::SharedClaudeConfig {
    let cfg = &project.config.agent.claude_code;
    Arc::new(tokio::sync::RwLock::new(
        control_plane::ClaudeRuntimeConfig {
            model: cfg.model.clone(),
            permission_mode: project::normalize_claude_permission_mode(&cfg.permission_mode),
            allowed_tools: cfg.allowed_tools.clone(),
        },
    ))
}

/// Configuration for `run_daemon_loop`.
struct DaemonConfig {
    bus: EventBus,
    project_root: PathBuf,
    autonomy: SharedAutonomy,
    shared_external_agent: Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>>,
    shared_codex_config: control_plane::SharedCodexConfig,
    shared_claude_config: control_plane::SharedClaudeConfig,
    frame_registry: Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    session_registry: Option<display::SharedSessionRegistry>,
    web_port: Option<u16>,
    flags_direct: bool,
    /// Optional shared session state for headless mode (cleared between tasks).
    shared_session: Option<web_gateway::SharedActiveSession>,
}

/// Daemon loop shared by the TUI post-exit path and the headless web-gateway path.
///
/// Waits for `StartTask` and `SetExternalAgent` control messages from the web
/// UI, spawning agent tasks in the background. Exits when the bus closes.
///
/// Ctrl+C is handled by the global signal handler installed in `main`, which
/// writes `mark_interrupted` to the session meta and calls `exit(130)` — we
/// deliberately do not also listen for it here because racing two handlers
/// risked the loop `break`ing before the meta update ran.
async fn run_daemon_loop(config: DaemonConfig) {
    session_supervisor::SessionSupervisor::new(session_supervisor::SessionSupervisorConfig {
        bus: config.bus,
        project_root: config.project_root,
        autonomy: config.autonomy,
        shared_external_agent: config.shared_external_agent,
        shared_codex_config: config.shared_codex_config,
        shared_claude_config: config.shared_claude_config,
        frame_registry: config.frame_registry,
        session_registry: config.session_registry,
        web_port: config.web_port,
        flags_direct: config.flags_direct,
        shared_session: config.shared_session,
        provider_factory: None,
    })
    .run()
    .await;
}

/// CLI flags parsed from command-line arguments.
struct CliFlags {
    task: Option<String>,
    /// --task-file <PATH>: Read the initial task from a file instead of argv.
    task_file: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    verbose: bool,
    no_tui: bool,
    mcp: bool,
    autonomy: AutonomyLevel,
    log_file: Option<String>,
    /// --continue / -c: resume the most recent session for this project.
    continue_last: bool,
    /// --resume / -r [id]: resume a specific session by ID or path.
    resume_id: Option<String>,
    control_socket: bool,
    /// --json: Emit JSONL events to stdout (implies --no-tui).
    json_output: bool,
    /// --sandbox: Enable Landlock filesystem sandboxing for the runtime.
    #[allow(dead_code)]
    sandbox: bool,
    /// --direct: Force single-agent mode (skip orchestrator/sub-agent delegation).
    /// Does NOT disable the UI — use --no-web --no-tui for headless output.
    direct: bool,
    /// --no-presence: Disable the presence layer (direct agent interaction).
    no_presence: bool,
    /// --web [PORT]: Serve TUI via web (xterm.js + optional voice).
    web: bool,
    web_port: u16,
    /// --bind <ADDR>: IP address for the web dashboard listener. Defaults
    /// to wildcard dual-stack when available. Use 127.0.0.1 with --no-tls
    /// for local automation.
    web_bind: Option<IpAddr>,
    /// --owner <CLIENT-KEY-FINGERPRINT>: seed a root grant pinned to that
    /// browser identity key at startup (the install.sh bootstrap: authority
    /// minted locally from first boot, no secrets on the wire).
    owner: Option<String>,
    /// --no-tls: Explicitly serve the web dashboard over plain HTTP. The
    /// dashboard defaults to mTLS; this flag is the debug/programmatic escape
    /// hatch for callers that knowingly want cleartext.
    no_tls: bool,
    /// --allow-public-plaintext: Acknowledge that --no-tls on a wildcard
    /// listener can expose the dashboard on public interfaces.
    allow_public_plaintext: bool,
    /// --tls: Serve the `--web` dashboard over HTTPS/WSS without requiring
    /// browser/client certificates. Installed access certs are preferred when
    /// present, otherwise a self-signed cert is minted at startup.
    tls: bool,
    /// --tls-cert <PATH>: PEM cert (chain) overriding default cert selection.
    /// Must be paired with `--tls-key`. Implies `--tls`.
    tls_cert: Option<String>,
    /// --tls-key <PATH>: PEM private key matching `--tls-cert`.
    tls_key: Option<String>,
    /// --mtls: Explicitly require a browser/client certificate signed by the
    /// configured client CA. This is also the default when web is enabled.
    mtls: bool,
    /// --mtls-ca <PATH>: PEM CA bundle used to verify client certs.
    /// Defaults to the installed access CA when present.
    mtls_ca: Option<String>,
    /// --transcription: Enable user speech transcription.
    transcription: bool,
    /// --record-display <ID>: Record an existing X11 display (repeatable).
    record_displays: Vec<u32>,

    /// --agent <BACKEND>: Use external agent backend (codex, claude-code).
    agent_backend: Option<external_agent::AgentBackend>,

    /// --no-web: Disable web gateway (on by default).
    no_web: bool,

    /// --advertise-url <URL>: WebSocket URL to advertise in this daemon's
    /// Agent Card (repeatable). Each occurrence appends one URL in the
    /// preference order they're given. When non-empty, the entire list
    /// replaces both the `[server.advertise]` config value and the
    /// auto-detected single URL — operator at the CLI wins.
    advertise_urls: Vec<String>,
}

fn print_help() {
    println!("intendant - AI agent runtime with process lifecycle management");
    println!();
    println!("USAGE:");
    println!("    intendant [OPTIONS] [TASK]");
    println!("    echo \"task\" | intendant [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("    --provider <NAME>     API provider (openai, anthropic, or gemini)");
    println!("    --model <NAME>        Model name to use");
    println!("    --task-file <PATH>    Read initial task from file instead of argv");
    println!("    --autonomy <LEVEL>    Autonomy level: low, medium, high, full");
    println!("    --log-file <DIR>      Override session log directory (default: ~/.intendant/logs/<uuid>/)");
    println!("    --continue, -c        Resume the most recent session for this project");
    println!("    --resume, -r [ID]     Resume a specific session by ID, prefix, or path");
    println!("    --no-tui              Disable terminal TUI; combine with --no-web for headless");
    println!("    --mcp                 Run as MCP server on stdio (replaces TUI)");
    println!("    --verbose, -v         Enable verbose output");
    println!("    --control-socket      Enable Unix control socket");
    println!("    --json                Emit JSONL events to stdout (implies --no-tui)");
    println!("    --sandbox             Enable Landlock filesystem sandboxing for the runtime");
    println!("    --direct              Force single-agent mode (skip orchestrator/sub-agent delegation)");
    println!("    --no-presence         Disable the presence layer (direct agent interaction)");
    println!("    --web [PORT]          Web dashboard (default: on, port 8765; idle starts daemon/no TUI)");
    println!("    --bind <ADDR>         IP address for the web dashboard listener");
    println!("    --owner <FINGERPRINT> Pin root authority to a browser client key at startup (install bootstrap)");
    println!(
        "    --no-tls              Serve the web dashboard over plain HTTP (explicit debug escape)"
    );
    println!("    --allow-public-plaintext  Allow --no-tls wildcard bind when public IPs exist");
    println!(
        "    --tls                 Serve HTTPS/WSS without requiring browser client certificates"
    );
    println!("    --tls-cert <PATH>     PEM cert overriding default cert selection (with --tls-key; implies --tls)");
    println!("    --tls-key <PATH>      PEM private key matching --tls-cert");
    println!(
        "    --mtls                Require client certificates signed by the Intendant access CA (default)"
    );
    println!("    --mtls-ca <PATH>      PEM CA bundle for --mtls client certificate verification");
    println!("    --no-web              Disable web dashboard; use terminal TUI when interactive");
    println!("    --transcription       Enable user speech transcription");
    println!(
        "    --record-display <ID> Record an existing X11 display (e.g. 50 for :50, repeatable)"
    );
    println!("    --agent <BACKEND>     Use external agent backend (codex, claude-code)");
    println!("    --advertise-url <URL> WebSocket URL to advertise to peers in this daemon's");
    println!("                          Agent Card (repeatable, preference order). Overrides");
    println!("                          [server.advertise] in intendant.toml when given.");
    println!("                          Example: --advertise-url wss://192.168.1.42:8765/ws");
    println!(
        "                                   --advertise-url wss://node.tail-abcd.ts.net:8443/ws"
    );
    println!("    --help, -h            Show this help message");
    println!();
    println!("SUBCOMMANDS:");
    println!("    ctl                   Control a running Intendant daemon over MCP");
    println!("    access                Configure dashboard TLS/mTLS access certificates");
    println!("    org                   Create or print a local org root key");
    println!("    peer                  Pair and configure federated Intendant peers");
    println!("    service               Install, remove, inspect, or run the boot service");
    println!("    setup                 Install or verify host-level Intendant dependencies");
    println!();
    println!("SESSION LOGS:");
    println!(
        "    Logs are always written to ~/.intendant/logs/<uuid>/ (override with --log-file)."
    );
    println!("    The log directory contains:");
    println!("      session.jsonl           Structured JSONL event log (one JSON object per line)");
    println!("      turns/turn_NNN_*.txt    Full model responses, agent I/O per turn");
    println!("      summary.json            Post-session summary");
    println!();
    println!("    AI agents can grep session.jsonl by event type, turn number, or level,");
    println!("    then read specific turn files for full content.");
    println!();
    println!("ENVIRONMENT:");
    println!("    OPENAI_API_KEY        OpenAI API key (for openai provider)");
    println!("    ANTHROPIC_API_KEY     Anthropic API key (for anthropic provider)");
    println!("    GEMINI_API_KEY        Google AI API key (for gemini provider)");
    println!("    PROVIDER              Default provider (openai, anthropic, or gemini)");
    println!("    MODEL_NAME            Default model name");
    println!("    STRUCTURED_OUTPUT     Enable JSON structured output (true/false)");
    println!("    REASONING_EFFORT      Reasoning effort: low, medium, high");
    println!("    REASONING_SUMMARY     Reasoning summary: auto, concise, detailed");
}

fn parse_cli_flags() -> Result<CliFlags, CallerError> {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut flags = CliFlags {
        task: None,
        task_file: None,
        provider: None,
        model: None,
        verbose: false,
        no_tui: false,
        mcp: false,
        autonomy: AutonomyLevel::Medium,
        log_file: None,
        continue_last: false,
        resume_id: None,
        control_socket: false,
        json_output: false,
        sandbox: false,
        direct: false,
        no_presence: false,
        web: false,
        web_port: web_gateway::DEFAULT_PORT,
        web_bind: None,
        owner: None,
        no_tls: false,
        allow_public_plaintext: false,
        tls: false,
        tls_cert: None,
        tls_key: None,
        mtls: false,
        mtls_ca: None,
        transcription: false,
        record_displays: Vec::new(),

        agent_backend: None,

        no_web: false,

        advertise_urls: Vec::new(),
    };

    let mut task_parts = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--provider" => {
                if i + 1 < args.len() {
                    flags.provider = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    return Err(CallerError::Config(
                        "Missing value for --provider".to_string(),
                    ));
                }
            }
            "--model" => {
                if i + 1 < args.len() {
                    flags.model = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    return Err(CallerError::Config("Missing value for --model".to_string()));
                }
            }
            "--task-file" => {
                if i + 1 < args.len() {
                    flags.task_file = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    return Err(CallerError::Config(
                        "Missing value for --task-file".to_string(),
                    ));
                }
            }
            "--verbose" | "-v" => {
                flags.verbose = true;
                i += 1;
            }
            "--no-tui" => {
                flags.no_tui = true;
                i += 1;
            }
            "--autonomy" => {
                if i + 1 < args.len() {
                    flags.autonomy = AutonomyLevel::from_str_loose(&args[i + 1]);
                    i += 2;
                } else {
                    return Err(CallerError::Config(
                        "Missing value for --autonomy".to_string(),
                    ));
                }
            }
            "--log-file" => {
                if i + 1 < args.len() {
                    flags.log_file = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    return Err(CallerError::Config(
                        "Missing value for --log-file".to_string(),
                    ));
                }
            }
            "--continue" | "-c" => {
                flags.continue_last = true;
                i += 1;
            }
            "--resume" | "-r" => {
                if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    flags.resume_id = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    // --resume without argument acts like --continue
                    flags.continue_last = true;
                    i += 1;
                }
            }
            "--mcp" => {
                flags.mcp = true;
                i += 1;
            }
            "--json" => {
                flags.json_output = true;
                flags.no_tui = true; // --json implies --no-tui
                i += 1;
            }
            "--sandbox" => {
                flags.sandbox = true;
                i += 1;
            }
            "--control-socket" => {
                flags.control_socket = true;
                i += 1;
            }
            "--direct" => {
                flags.direct = true;
                i += 1;
            }
            "--no-presence" => {
                flags.no_presence = true;
                i += 1;
            }
            "--no-web" => {
                flags.no_web = true;
                i += 1;
            }
            "--web" => {
                flags.web = true;
                // --web enables the dashboard. Idle web startup uses the
                // daemon/no-terminal-TUI path; a task still runs through the
                // normal frontend selection below.
                // Optional port argument (next arg if it's numeric)
                if i + 1 < args.len() && args[i + 1].parse::<u16>().is_ok() {
                    flags.web_port = args[i + 1].parse().unwrap();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--bind" => {
                if i + 1 < args.len() {
                    let ip = args[i + 1].parse::<IpAddr>().map_err(|_| {
                        CallerError::Config(format!(
                            "--bind: '{}' is not a valid IP address",
                            args[i + 1]
                        ))
                    })?;
                    flags.web_bind = Some(ip);
                    i += 2;
                } else {
                    return Err(CallerError::Config("Missing value for --bind".to_string()));
                }
            }
            "--owner" => {
                if i + 1 < args.len() {
                    // Fail a typo at the flag, not after it's pinned: an
                    // install whose owner fingerprint is garbage is an
                    // unclaimable box that believes it's owned.
                    let value = args[i + 1].trim().to_string();
                    if !access::client_key::is_client_key_fingerprint(&value) {
                        let shown: String = if value.chars().count() > 48 {
                            value.chars().take(48).chain("…".chars()).collect()
                        } else {
                            value.clone()
                        };
                        return Err(CallerError::Config(format!(
                            "--owner: '{shown}' is not a client-key fingerprint (expected 43 \
                             base64url characters — copy it from the dashboard's Access drawer)"
                        )));
                    }
                    flags.owner = Some(value);
                    i += 2;
                } else {
                    return Err(CallerError::Config(
                        "Missing value for --owner (a client-key fingerprint)".to_string(),
                    ));
                }
            }
            "--no-tls" => {
                flags.no_tls = true;
                i += 1;
            }
            "--allow-public-plaintext" => {
                flags.allow_public_plaintext = true;
                i += 1;
            }
            "--tls" => {
                // Serve the dashboard over HTTPS/WSS. Installed access certs
                // are preferred unless --tls-cert/--tls-key override them.
                flags.tls = true;
                i += 1;
            }
            "--tls-cert" => {
                if i + 1 < args.len() {
                    flags.tls_cert = Some(args[i + 1].clone());
                    flags.tls = true; // a cert override implies TLS
                    i += 2;
                } else {
                    return Err(CallerError::Config(
                        "Missing value for --tls-cert".to_string(),
                    ));
                }
            }
            "--tls-key" => {
                if i + 1 < args.len() {
                    flags.tls_key = Some(args[i + 1].clone());
                    flags.tls = true; // a key override implies TLS
                    i += 2;
                } else {
                    return Err(CallerError::Config(
                        "Missing value for --tls-key".to_string(),
                    ));
                }
            }
            "--mtls" => {
                flags.mtls = true;
                flags.tls = true; // mTLS necessarily implies TLS.
                i += 1;
            }
            "--mtls-ca" => {
                if i + 1 < args.len() {
                    flags.mtls_ca = Some(args[i + 1].clone());
                    flags.mtls = true;
                    flags.tls = true;
                    i += 2;
                } else {
                    return Err(CallerError::Config(
                        "Missing value for --mtls-ca".to_string(),
                    ));
                }
            }
            "--transcription" => {
                flags.transcription = true;
                i += 1;
            }
            "--agent" => {
                if i + 1 < args.len() {
                    let backend = external_agent::AgentBackend::from_str_loose(&args[i + 1])
                        .ok_or_else(|| {
                            CallerError::Config(format!(
                                "Unknown agent backend: '{}'. Valid options: codex, claude-code",
                                args[i + 1]
                            ))
                        })?;
                    flags.agent_backend = Some(backend);
                    i += 2;
                } else {
                    return Err(CallerError::Config("Missing value for --agent".to_string()));
                }
            }
            "--advertise-url" => {
                // Repeatable: every occurrence appends one URL in the
                // order given. The full list replaces config + auto-
                // detection when non-empty.
                if i + 1 < args.len() {
                    flags.advertise_urls.push(args[i + 1].clone());
                    i += 2;
                } else {
                    return Err(CallerError::Config(
                        "Missing value for --advertise-url".to_string(),
                    ));
                }
            }
            "--record-display" => {
                if i + 1 >= args.len() {
                    return Err(CallerError::Config(
                        "--record-display requires a display ID (e.g. 50 for :50)".to_string(),
                    ));
                }
                let raw = args[i + 1].trim_start_matches(':');
                let id: u32 = raw.parse().map_err(|_| {
                    CallerError::Config(format!(
                        "--record-display: '{}' is not a valid display ID",
                        args[i + 1]
                    ))
                })?;
                flags.record_displays.push(id);
                i += 2;
            }
            other => {
                if other.starts_with('-') {
                    return Err(CallerError::Config(format!(
                        "Unknown CLI flag: {}. Use --help to see valid options.",
                        other
                    )));
                }
                task_parts.push(other.to_string());
                i += 1;
            }
        }
    }

    if !task_parts.is_empty() {
        flags.task = Some(task_parts.join(" "));
    }
    if flags.task.is_some() && flags.task_file.is_some() {
        return Err(CallerError::Config(
            "`--task-file` cannot be combined with a positional task".to_string(),
        ));
    }
    validate_tls_cli_flags(&flags)?;

    Ok(flags)
}

/// Wire the fission branch lifecycle into a startup path: spawn the bus
/// watcher that feeds branch session lifecycle/diff events into the durable
/// fission ledger, and rehydrate routes for branches that were still running
/// when the previous process exited. Every startup path that can host a
/// managed Codex conversation (and therefore a `fission_spawn`) must call
/// this, or spawned branches complete without their ledger statuses ever
/// flipping.
fn start_fission_lifecycle(
    bus: &EventBus,
    session_log: &SharedSessionLog,
) -> tokio::task::JoinHandle<()> {
    let watcher = fission_lifecycle::spawn_fission_lifecycle_watcher(bus.subscribe());
    if let Some(home) = dirs::home_dir() {
        match fission_lifecycle::rehydrate_from_logs(&home.join(".intendant").join("logs")) {
            Ok(0) => {}
            Ok(count) => slog(session_log, |l| {
                l.info(&format!(
                    "Rehydrated {count} fission branch route(s) from persisted ledgers"
                ))
            }),
            Err(err) => slog(session_log, |l| {
                l.warn(&format!("Fission branch route rehydration failed: {err}"))
            }),
        }
    }
    watcher
}

fn extract_json(text: &str) -> Option<&str> {
    // Try to find JSON in ```json code fences
    if let Some(start) = text.find("```json") {
        let json_start = start + 7;
        if let Some(end) = text[json_start..].find("```") {
            let candidate = text[json_start..json_start + end].trim();
            if candidate.starts_with('{') {
                return Some(candidate);
            }
        }
    }

    // Try generic code fences
    if let Some(start) = text.find("```") {
        let after_fence = start + 3;
        let json_start = if let Some(nl) = text[after_fence..].find('\n') {
            after_fence + nl + 1
        } else {
            after_fence
        };
        if let Some(end) = text[json_start..].find("```") {
            let candidate = text[json_start..json_start + end].trim();
            if candidate.starts_with('{') {
                return Some(candidate);
            }
        }
    }

    // Try bare JSON - find first { and last }
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            if end > start {
                let candidate = &text[start..=end];
                if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

/// Parse a `BRIEF: ...` line from the model's last response.
/// Returns `(brief_text, was_explicit)` — `was_explicit` is false when falling back.
fn parse_brief(text: &str) -> (String, bool) {
    // Look for explicit BRIEF: marker (scan from end for last occurrence)
    for line in text.lines().rev() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("BRIEF:") {
            let brief = rest.trim();
            if !brief.is_empty() {
                return (brief.to_string(), true);
            }
        }
    }
    // Fallback: extract first 1-2 sentences from the text
    (extract_brief_from_text(text), false)
}

/// Extract a short brief from freeform text by taking the first 1-2 sentences.
fn extract_brief_from_text(text: &str) -> String {
    let cleaned = text.trim();
    if cleaned.is_empty() {
        return "Task completed.".to_string();
    }
    // Skip markdown headers and blank lines to find the first content line(s)
    let mut sentences = String::new();
    let mut sentence_count = 0;
    for line in cleaned.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("```")
            || trimmed.starts_with("BRIEF:")
        {
            if sentence_count > 0 {
                break; // Stop at first blank/header after content
            }
            continue;
        }
        // Strip markdown formatting
        let plain = trimmed
            .trim_start_matches("- ")
            .trim_start_matches("* ")
            .trim_start_matches("> ");
        if !sentences.is_empty() {
            sentences.push(' ');
        }
        sentences.push_str(plain);
        sentence_count += 1;
        if sentence_count >= 2 || sentences.len() > 200 {
            break;
        }
    }
    if sentences.is_empty() {
        return "Task completed.".to_string();
    }
    // Truncate if still too long
    if sentences.len() > 200 {
        let cut = char_boundary_at_or_before(&sentences, 200);
        if let Some(pos) = sentences[..cut].rfind(". ") {
            sentences.truncate(pos + 1);
        } else {
            sentences.truncate(cut);
            sentences.push_str("...");
        }
    }
    sentences
}

/// Returns (json_string, had_context_directives).
/// Empty json_string means no commands to execute.
fn apply_context_directives(json_str: &str, conversation: &mut Conversation) -> (String, bool) {
    let mut value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return (json_str.to_string(), false),
    };

    let mut had_context = false;

    if let Some(context) = value.get("context").cloned() {
        had_context = true;

        // Apply drop_turns
        if let Some(drops) = context.get("drop_turns").and_then(|d| d.as_array()) {
            let indices: Vec<usize> = drops
                .iter()
                .filter_map(|v| v.as_u64().map(|n| n as usize))
                .collect();
            conversation.drop_turns(&indices);
        }

        // Apply summarize
        if let Some(summarize) = context.get("summarize") {
            if let (Some(turns), Some(summary)) = (
                summarize.get("turns").and_then(|t| t.as_array()),
                summarize.get("summary").and_then(|s| s.as_str()),
            ) {
                let indices: Vec<usize> = turns
                    .iter()
                    .filter_map(|v| v.as_u64().map(|n| n as usize))
                    .collect();
                conversation.summarize_turns(&indices, summary);
            }
        }

        // Strip context field before passing to agent
        if let Some(obj) = value.as_object_mut() {
            obj.remove("context");
        }
    }

    // Check if there are commands; if not, return empty to signal no commands
    let has_commands = value
        .get("commands")
        .and_then(|c| c.as_array())
        .is_some_and(|a| !a.is_empty());

    if !has_commands {
        return (String::new(), had_context);
    }

    (
        serde_json::to_string(&value).unwrap_or_else(|_| json_str.to_string()),
        had_context,
    )
}

fn inject_project_context(json_str: &str, project: &Project) -> String {
    let mut value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return json_str.to_string(),
    };

    if let Some(commands) = value.get_mut("commands").and_then(|c| c.as_array_mut()) {
        let memory_file = project.memory_path().to_string_lossy().to_string();

        for cmd in commands.iter_mut() {
            if let Some("storeMemory" | "recallMemory") =
                cmd.get("function").and_then(|f| f.as_str())
            {
                if cmd.get("memory_file").is_none() {
                    cmd["memory_file"] = serde_json::Value::String(memory_file.clone());
                }
            }
        }
    }

    serde_json::to_string(&value).unwrap_or_else(|_| json_str.to_string())
}

fn has_ask_human_command(json_str: &str) -> bool {
    let parsed: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return false,
    };

    parsed
        .get("commands")
        .and_then(|v| v.as_array())
        .map(|commands| {
            commands
                .iter()
                .any(|cmd| cmd.get("function").and_then(|v| v.as_str()) == Some("askHuman"))
        })
        .unwrap_or(false)
}

/// Extract the question text from an askHuman command in a batch JSON string.
fn extract_ask_human_question(json_str: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(json_str).ok()?;
    parsed
        .get("commands")
        .and_then(|v| v.as_array())
        .and_then(|commands| {
            commands.iter().find_map(|cmd| {
                if cmd.get("function").and_then(|v| v.as_str()) == Some("askHuman") {
                    cmd.get("question")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
        })
}

fn has_capture_screen_command(json_str: &str) -> bool {
    let parsed: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return false,
    };

    parsed
        .get("commands")
        .and_then(|v| v.as_array())
        .map(|commands| {
            commands
                .iter()
                .any(|cmd| cmd.get("function").and_then(|v| v.as_str()) == Some("captureScreen"))
        })
        .unwrap_or(false)
}

fn has_exec_command(json_str: &str) -> bool {
    let parsed: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return false,
    };

    parsed
        .get("commands")
        .and_then(|v| v.as_array())
        .map(|commands| {
            commands.iter().any(|cmd| {
                matches!(
                    cmd.get("function").and_then(|v| v.as_str()),
                    Some("execAsAgent" | "execPty")
                )
            })
        })
        .unwrap_or(false)
}

/// Try to encode a captureScreen result as base64 image data.
/// Returns `Some(vec![ImageData])` on success, `None` on any failure.
fn encode_screenshot(result_text: &str) -> Option<Vec<conversation::ImageData>> {
    let parsed: serde_json::Value = serde_json::from_str(result_text).ok()?;
    if parsed.get("success").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let path_str = parsed.get("screenshot_path").and_then(|v| v.as_str())?;
    let bytes = std::fs::read(path_str).ok()?;
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(vec![conversation::ImageData {
        media_type: "image/png".to_string(),
        data: encoded,
    }])
}

/// Auto-launch Xvfb when no working display exists and the batch needs one.
///
/// Detection flow:
/// 1. Already launched (`xvfb_guard` is `Some`)? → skip
/// 2. Current DISPLAY accessible? Yes → skip
/// 3. Batch contains `captureScreen` or any `execAsAgent`? No → skip
/// 4. Launch Xvfb, store guard, set DISPLAY
/// 5. On failure → log warning, let commands fail naturally
///
/// Format raw agent JSON into a human-readable preview for the Activity tab.
pub(crate) fn format_commands_preview(json_str: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
        if let Some(cmds) = parsed.get("commands").and_then(|v| v.as_array()) {
            let parts: Vec<String> = cmds
                .iter()
                .filter_map(|cmd| {
                    let func = cmd.get("function").and_then(|v| v.as_str()).unwrap_or("?");
                    match func {
                        "execAsAgent" => cmd
                            .get("command")
                            .and_then(|v| v.as_str())
                            .map(|c| format!("exec: {}", c)),
                        "inspectPath" => cmd
                            .get("path")
                            .and_then(|v| v.as_str())
                            .map(|p| format!("inspect: {}", p)),
                        "editFile" | "writeFile" => cmd
                            .get("file_path")
                            .and_then(|v| v.as_str())
                            .map(|p| format!("{}: {}", func, p)),
                        "spawn_live_audio" => Some(format!(
                            "spawn_live_audio ({})",
                            cmd.get("provider").and_then(|v| v.as_str()).unwrap_or("?")
                        )),
                        _ => Some(func.to_string()),
                    }
                })
                .collect();
            if !parts.is_empty() {
                return parts.join(" | ");
            }
        }
    }
    json_str.to_string()
}

/// We launch on execAsAgent (not just captureScreen) because GUI applications
/// started in early turns must share the same display that captureScreen will
/// later capture. Launching only on captureScreen is too late — the app would
/// already be running on a different (or no) display.
async fn maybe_auto_launch_xvfb(
    json_str: &str,
    xvfb_guard: &mut Option<vision::XvfbGuard>,
    provider_name: &str,
    session_log: &SharedSessionLog,
) {
    if xvfb_guard.is_some() {
        return;
    }
    if !has_capture_screen_command(json_str) && !has_exec_command(json_str) {
        return;
    }
    // If a display is already accessible (e.g. DISPLAY was set before launch,
    // or on macOS where the native display is always available), skip Xvfb.
    // Don't emit DisplayReady — no DisplaySession exists, so the web dashboard
    // can't connect via WebRTC. Recording uses the legacy platform ffmpeg path
    // directly (x11grab on Linux, screencapture/image2pipe on macOS).
    if vision::is_display_accessible() {
        let default_display = if cfg!(target_os = "macos") { 0 } else { 99 };
        let display_id = std::env::var("DISPLAY")
            .ok()
            .and_then(|d| d.trim_start_matches(':').parse::<u32>().ok())
            .unwrap_or(default_display);
        let (width, height) = query_display_resolution(display_id);
        slog(session_log, |l| {
            l.info(&format!(
                "Using existing display :{} ({}x{}) — no web slot (no DisplaySession)",
                display_id, width, height
            ))
        });
        return;
    }
    let config = vision::display_config_for_provider(provider_name);
    let trigger = if has_capture_screen_command(json_str) {
        "captureScreen"
    } else {
        "execAsAgent (display needed)"
    };
    let virtual_id = match config.target {
        computer_use::DisplayTarget::Virtual { id } => id,
        _ => return,
    };
    slog(session_log, |l| {
        l.info(&format!(
            "Auto-launching Xvfb :{} at {}x{} for {}",
            virtual_id, config.width, config.height, trigger
        ))
    });
    match vision::launch_display(&config).await {
        Ok(guard) => {
            // Phase 1: no DisplayReady for virtual displays — no DisplaySession means no web slot.
            // The agent uses this display for CU via X11 tools directly.
            slog(session_log, |l| {
                l.info(&format!(
                    "Xvfb :{} launched (no web slot in phase 1)",
                    virtual_id
                ))
            });
            *xvfb_guard = Some(guard);
        }
        Err(e) => {
            slog(session_log, |l| {
                l.warn(&format!("Failed to auto-launch Xvfb: {}", e))
            });
        }
    }
}

/// Query the resolution of the native display via system_profiler.
/// Returns the logical (point) resolution, not device pixels.
/// Uses CoreGraphics via swift, which returns logical resolution directly
/// (e.g. 1339x837 on a Retina display, not the 2x device pixel size).
/// Falls back to system_profiler, then a default.
#[cfg(target_os = "macos")]
pub(crate) fn query_display_resolution(_display_id: u32) -> (u32, u32) {
    // Primary method: CoreGraphics (works in VMs where system_profiler is empty)
    if let Ok(out) = std::process::Command::new("swift")
        .args(["-e", "import CoreGraphics; let d = CGMainDisplayID(); print(\"\\(CGDisplayPixelsWide(d))x\\(CGDisplayPixelsHigh(d))\")"])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let parts: Vec<&str> = text.split('x').collect();
        if parts.len() == 2 {
            if let (Ok(w), Ok(h)) = (parts[0].parse(), parts[1].parse()) {
                return (w, h);
            }
        }
    }
    // Fallback: system_profiler (may be empty in VMs)
    if let Ok(out) = std::process::Command::new("system_profiler")
        .arg("SPDisplaysDataType")
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("Resolution:") {
                let parts: Vec<&str> = trimmed.split_whitespace().collect();
                if parts.len() >= 4 {
                    if let (Ok(w), Ok(h)) = (parts[1].parse::<u32>(), parts[3].parse::<u32>()) {
                        let is_retina = trimmed.to_lowercase().contains("retina");
                        if is_retina {
                            return (w / 2, h / 2);
                        }
                        return (w, h);
                    }
                }
            }
        }
    }
    (1920, 1080)
}

/// Query the resolution of an existing X11 display via xdpyinfo.
/// Returns (width, height) or a default of (1280, 720) if detection fails.
#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
pub(crate) fn query_display_resolution(display_id: u32) -> (u32, u32) {
    let output = std::process::Command::new("xdpyinfo")
        .arg("-display")
        .arg(format!(":{}", display_id))
        .output();
    if let Ok(out) = output {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("dimensions:") {
                // "dimensions:    1280x720 pixels (338x190 millimeters)"
                if let Some(dims) = trimmed.split_whitespace().nth(1) {
                    let parts: Vec<&str> = dims.split('x').collect();
                    if parts.len() == 2 {
                        if let (Ok(w), Ok(h)) = (parts[0].parse(), parts[1].parse()) {
                            return (w, h);
                        }
                    }
                }
            }
        }
    }
    (1280, 720)
}

/// No X11 / `xdpyinfo` on Windows. Return the same conservative default
/// the X11 path falls back to; Tier-1's DXGI backend will report the real
/// resolution via the display enumeration path instead.
#[cfg(target_os = "windows")]
pub(crate) fn query_display_resolution(_display_id: u32) -> (u32, u32) {
    (1280, 720)
}

/// Start recording external displays (--record-display) directly on the registry.
/// Does NOT emit DisplayReady — external displays have no DisplaySession, so the
/// web dashboard can't connect. Recording uses x11grab independently.
async fn start_external_display_recordings(
    displays: &[u32],
    registry: &std::sync::Arc<tokio::sync::RwLock<recording::RecordingRegistry>>,
    bus: &EventBus,
) {
    for &id in displays {
        let (width, height) = query_display_resolution(id);
        eprintln!("Recording external display :{} ({}x{})", id, width, height);
        let mut reg = registry.write().await;
        if !reg.is_enabled() {
            eprintln!("Recording not enabled in config — skipping :{}", id);
            continue;
        }
        if !recording::is_ffmpeg_available() {
            eprintln!("ffmpeg not available — skipping :{}", id);
            continue;
        }
        match reg.start_external_display(id, width, height).await {
            Ok(stream_name) => {
                bus.send(AppEvent::RecordingStarted { stream_name });
            }
            Err(e) => eprintln!("Failed to start recording for :{}: {}", id, e),
        }
    }
}

/// Side effects a user approval carries beyond unblocking the waiting
/// command: dedup recording for plain approvals, autonomy escalation for
/// approve-all, and the first DisplayControl approval granting user-display
/// access session-wide. Every approval surface (JSON stdin slot, TUI/MCP
/// registry) must apply these identically, or an approval "succeeds" and
/// the action still fails its grant check afterwards.
/// Shared side effects for NATIVE runtime approvals, applied identically
/// by every surface (TUI Enter, web, MCP): Approve records the command
/// for dedup, ApproveAll raises global autonomy to Full, DisplayControl
/// grants user display access.
///
/// External-agent approvals deliberately do NOT route here: their
/// "Approve all" is Intendant-enforced per external session
/// (`approve_all_session` in the agent event loop) instead of flipping
/// global autonomy — a button on one Codex/Claude session must not
/// escalate every other surface of the daemon.
async fn apply_user_approval(
    response: event::ApprovalResponse,
    cat: autonomy::ActionCategory,
    preview: &str,
    autonomy: &SharedAutonomy,
    bus: &EventBus,
) {
    let mut state = autonomy.write().await;
    match response {
        event::ApprovalResponse::Approve => state.record_approved_command(preview),
        event::ApprovalResponse::ApproveAll => state.level = AutonomyLevel::Full,
        event::ApprovalResponse::Skip | event::ApprovalResponse::Deny => return,
    }
    if cat == autonomy::ActionCategory::DisplayControl && !state.user_display_granted {
        state.user_display_granted = true;
        std::env::set_var("INTENDANT_USER_DISPLAY_GRANTED", "1");
        bus.send(AppEvent::UserDisplayGranted { display_id: 0 });
    }
}

/// Format a human-readable command preview from raw JSON (for approval display).
fn format_command_preview(json_str: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(json_str) {
        if let Some(commands) = parsed.get("commands").and_then(|c| c.as_array()) {
            let summaries: Vec<String> = commands
                .iter()
                .map(|cmd| {
                    let func = cmd.get("function").and_then(|f| f.as_str()).unwrap_or("?");
                    match func {
                        "execAsAgent" => {
                            let command =
                                cmd.get("command").and_then(|c| c.as_str()).unwrap_or("?");
                            format!("exec: {}", command)
                        }
                        "writeFile" | "editFile" => {
                            let path = cmd.get("file_path").and_then(|p| p.as_str()).unwrap_or("?");
                            format!("{}: {}", func, path)
                        }
                        "inspectPath" => {
                            let path = cmd.get("path").and_then(|p| p.as_str()).unwrap_or("?");
                            format!("inspect: {}", path)
                        }
                        "browse" => {
                            let url = cmd.get("url").and_then(|u| u.as_str()).unwrap_or("?");
                            format!("browse: {}", url)
                        }
                        _ => func.to_string(),
                    }
                })
                .collect();
            if !summaries.is_empty() {
                return summaries.join(" | ");
            }
        }
    }
    // Fallback: full raw JSON (UI handles collapsing)
    json_str.to_string()
}

fn normalize_command_batch(json_str: &str) -> String {
    let mut value: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(_) => return json_str.to_string(),
    };

    let Some(commands) = value.get_mut("commands").and_then(|c| c.as_array_mut()) else {
        return json_str.to_string();
    };

    for cmd in commands {
        if cmd.get("function").and_then(|f| f.as_str()) == Some("writeFile") {
            cmd["function"] = serde_json::Value::String("editFile".to_string());
            if cmd.get("operation").is_none() {
                cmd["operation"] = serde_json::Value::String("write".to_string());
            }
        }
    }

    serde_json::to_string(&value).unwrap_or_else(|_| json_str.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn external_rollback_turn_in_progress_matches_codex_rpc_error() {
        let err = CallerError::ExternalAgent(
            "thread/rollback: External agent error: JSON-RPC error -32600: Cannot rollback while a turn is in progress"
                .to_string(),
        );
        assert!(external_rollback_turn_in_progress(&err));

        let unrelated = CallerError::ExternalAgent(
            "thread/rollback: External agent error: JSON-RPC error -32600: thread not found"
                .to_string(),
        );
        assert!(!external_rollback_turn_in_progress(&unrelated));
    }

    #[tokio::test]
    async fn resolve_attachments_includes_uploaded_files_and_images() {
        use std::io::Write as _;

        fn upload_tempfile(bytes: &[u8]) -> tempfile::NamedTempFile {
            let mut file = tempfile::NamedTempFile::new().unwrap();
            file.write_all(bytes).unwrap();
            file.flush().unwrap();
            file
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("session");
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();

        let file_upload = upload_store::commit_upload(
            upload_tempfile(b"a,b\n1,2\n"),
            "data.csv",
            "text/csv",
            8,
            upload_store::UploadDestination::Workspace,
            &session_dir,
            "sess-1",
            &project_root,
        )
        .unwrap();
        let image_upload = upload_store::commit_upload(
            upload_tempfile(b"not-really-a-png"),
            "screen.png",
            "image/png",
            16,
            upload_store::UploadDestination::Task,
            &session_dir,
            "sess-1",
            &project_root,
        )
        .unwrap();

        let registry = Arc::new(tokio::sync::RwLock::new(frames::FrameRegistry::new(
            &session_dir,
        )));
        let ids = vec![
            format!("upload:{}", file_upload.id),
            format!("upload:{}", image_upload.id),
        ];
        let attachments = resolve_attachments(&ids, &registry, &session_dir, &project_root).await;

        assert_eq!(attachments.len(), 2);
        match &attachments[0] {
            external_agent::AgentAttachment::File(file) => {
                assert_eq!(file.name, "data.csv");
                assert_eq!(file.mime_type, "text/csv");
                assert_eq!(file.size, 8);
                assert!(file.local_path.starts_with(
                    project_root
                        .join(".intendant")
                        .join("uploads")
                        .join("sess-1")
                ));
            }
            other => panic!("expected file upload attachment, got {other:?}"),
        }
        match &attachments[1] {
            external_agent::AgentAttachment::Image(image) => {
                assert_eq!(image.mime_type, "image/png");
                assert_eq!(image.local_path.as_ref(), Some(&image_upload.path));
                assert!(!image.base64.is_empty());
            }
            other => panic!("expected image upload attachment, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_attachments_falls_back_to_daemon_project_uploads() {
        use std::io::Write as _;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"pending upload").unwrap();
        file.flush().unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("new-session-log");
        let launch_project_root = tmp.path().join("launch-project");
        let daemon_project_root = tmp.path().join("daemon-project");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&launch_project_root).unwrap();
        std::fs::create_dir_all(&daemon_project_root).unwrap();

        let upload = upload_store::commit_upload(
            file,
            "pending.txt",
            "text/plain",
            14,
            upload_store::UploadDestination::Task,
            &session_dir,
            "daemon-session",
            &daemon_project_root,
        )
        .unwrap();

        let registry = Arc::new(tokio::sync::RwLock::new(frames::FrameRegistry::new(
            &session_dir,
        )));
        let ids = vec![format!("upload:{}", upload.id)];
        assert!(
            resolve_attachments(&ids, &registry, &session_dir, &launch_project_root)
                .await
                .is_empty(),
            "single-root lookup should not find uploads committed under another project"
        );

        let roots = vec![launch_project_root, daemon_project_root];
        let attachments =
            resolve_attachments_with_project_roots(&ids, &registry, &session_dir, &roots).await;

        assert_eq!(attachments.len(), 1);
        match &attachments[0] {
            external_agent::AgentAttachment::File(file) => {
                assert_eq!(file.name, "pending.txt");
                assert_eq!(file.local_path, upload.path);
            }
            other => panic!("expected fallback file upload attachment, got {other:?}"),
        }
    }

    #[test]
    fn parse_diff_file_paths_new_file() {
        let diff = "\
diff --git a/foo.rs b/foo.rs
new file mode 100644
index 0000000..abc
--- /dev/null
+++ b/foo.rs
@@ -0,0 +1,2 @@
+hello
+world
";
        let files = parse_diff_file_paths(diff);
        assert_eq!(files, vec!["foo.rs".to_string()]);
    }

    #[test]
    fn parse_diff_file_paths_absolute_with_double_slash() {
        // Codex in practice writes `b//home/user/...` for absolute paths.
        // The stripped form must preserve the leading `/`.
        let diff = "\
diff --git a//home/user/proj/x.py b//home/user/proj/x.py
new file mode 100644
--- /dev/null
+++ b//home/user/proj/x.py
@@ -0,0 +1 @@
+pass
";
        let files = parse_diff_file_paths(diff);
        assert_eq!(files, vec!["/home/user/proj/x.py".to_string()]);
    }

    #[test]
    fn parse_diff_file_paths_deleted_file() {
        // Pure deletion: `+++ /dev/null`, so we must pick up the `a/` side.
        let diff = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-removed line
";
        let files = parse_diff_file_paths(diff);
        assert_eq!(files, vec!["gone.txt".to_string()]);
    }

    #[test]
    fn parse_diff_file_paths_multiple_and_dedup() {
        let diff = "\
--- a/one.rs
+++ b/one.rs
@@ -1 +1 @@
-a
+b
--- a/two.rs
+++ b/two.rs
@@ -1 +1 @@
-x
+y
";
        let files = parse_diff_file_paths(diff);
        assert_eq!(files, vec!["one.rs".to_string(), "two.rs".to_string()]);
    }

    #[test]
    fn split_unified_diff_by_file_keeps_file_blocks() {
        let diff = "\
diff --git a/one.rs b/one.rs
--- a/one.rs
+++ b/one.rs
@@ -1 +1 @@
-a
+b
diff --git a/two.rs b/two.rs
--- a/two.rs
+++ b/two.rs
@@ -1 +1 @@
-x
+y
";
        let blocks = split_unified_diff_by_file(diff);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].0, "one.rs");
        assert!(blocks[0].1.contains("diff --git a/one.rs b/one.rs"));
        assert!(!blocks[0].1.contains("diff --git a/two.rs b/two.rs"));
        assert_eq!(blocks[1].0, "two.rs");
        assert!(blocks[1].1.contains("diff --git a/two.rs b/two.rs"));
    }

    #[test]
    fn resolve_diff_file_path_allows_project_and_tmp_absolute_paths() {
        // Platform-absolute fixture paths: `/work/project` is not absolute
        // on Windows, so prefix a drive there.
        fn abs(p: &str) -> PathBuf {
            if cfg!(windows) {
                PathBuf::from(format!("C:{}", p.replace('/', "\\")))
            } else {
                PathBuf::from(p)
            }
        }
        let project_root = abs("/work/project");
        let inside = abs("/work/project/src/main.rs");
        assert_eq!(
            resolve_diff_file_path(&project_root, inside.to_str().unwrap()).unwrap(),
            inside
        );
        let temp_file = std::env::temp_dir().join("intendant-edit.txt");
        assert_eq!(
            resolve_diff_file_path(&project_root, temp_file.to_str().unwrap()).unwrap(),
            temp_file
        );
        #[cfg(unix)]
        assert_eq!(
            resolve_diff_file_path(&project_root, "/tmp/intendant-edit.txt").unwrap(),
            PathBuf::from("/tmp/intendant-edit.txt")
        );
        let outside = abs("/etc/passwd");
        assert!(resolve_diff_file_path(&project_root, outside.to_str().unwrap()).is_none());
        assert!(resolve_diff_file_path(&project_root, "../outside.txt").is_none());
    }

    #[test]
    fn parse_session_diff_file_paths_reads_persisted_diff_logs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let jsonl = r#"{"event":"info","message":"External agent diff: one.rs\n--- a/one.rs\n+++ b/one.rs\n@@ -1 +1 @@\n-a\n+b\n"}"#;
        std::fs::write(tmp.path().join("session.jsonl"), format!("{jsonl}\n")).unwrap();

        let files = parse_session_diff_file_paths(tmp.path());
        assert_eq!(files, vec!["one.rs".to_string()]);
    }

    #[test]
    fn external_diff_delta_tracker_can_seed_resumed_session_state() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project_root = tmp.path().join("project");
        let log_dir = tmp.path().join("session");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(project_root.join("tracked.txt"), "old logged state\n").unwrap();
        let jsonl = r#"{"event":"info","message":"External agent diff: tracked.txt\n--- a/tracked.txt\n+++ b/tracked.txt\n@@ -0,0 +1 @@\n+old logged state\n"}"#;
        std::fs::write(log_dir.join("session.jsonl"), format!("{jsonl}\n")).unwrap();

        let mut tracker = ExternalDiffDeltaTracker::default();
        tracker.seed_from_session_log(&project_root, &log_dir);

        std::fs::write(
            project_root.join("tracked.txt"),
            "old logged state\nnew resumed edit\n",
        )
        .unwrap();
        let cumulative_after_resume = "\
diff --git a/tracked.txt b/tracked.txt
--- /dev/null
+++ b/tracked.txt
@@ -0,0 +1,2 @@
+old logged state
+new resumed edit
";
        let delta = tracker
            .delta(&project_root, &[], cumulative_after_resume)
            .unwrap();
        assert_eq!(delta.files_changed, vec!["tracked.txt".to_string()]);
        assert!(delta.unified_diff.contains("+new resumed edit"));
        assert!(!delta.unified_diff.contains("+old logged state"));
    }

    #[test]
    fn external_diff_delta_tracker_emits_per_event_changes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project_root = tmp.path();
        let mut tracker = ExternalDiffDeltaTracker::default();

        let smoke_delete = "\
diff --git a/activity-diff-smoke.txt b/activity-diff-smoke.txt
deleted file mode 100644
--- a/activity-diff-smoke.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-old one
-old two
";
        let first = tracker.delta(project_root, &[], smoke_delete).unwrap();
        assert_eq!(
            first.files_changed,
            vec!["activity-diff-smoke.txt".to_string()]
        );
        assert!(first.unified_diff.contains("activity-diff-smoke.txt"));
        assert!(first.unified_diff.contains("-old one"));

        std::fs::write(
            project_root.join("activity-diff-live-check.md"),
            "# Activity Diff Live Check\n\n- first event\n",
        )
        .unwrap();
        let cumulative_after_create = format!(
            "{}{}",
            smoke_delete,
            "\
diff --git a/activity-diff-live-check.md b/activity-diff-live-check.md
new file mode 100644
--- /dev/null
+++ b/activity-diff-live-check.md
@@ -0,0 +1,3 @@
+# Activity Diff Live Check
+
+- first event
"
        );
        let second = tracker
            .delta(project_root, &[], &cumulative_after_create)
            .unwrap();
        assert_eq!(
            second.files_changed,
            vec!["activity-diff-live-check.md".to_string()]
        );
        assert!(!second.unified_diff.contains("activity-diff-smoke.txt"));
        assert!(second.unified_diff.contains("activity-diff-live-check.md"));
        assert!(second.unified_diff.contains("+- first event"));

        std::fs::write(
            project_root.join("activity-diff-live-check.md"),
            "# Activity Diff Live Check\n\n- first event\n- second event\n",
        )
        .unwrap();
        let cumulative_after_modify = format!(
            "{}{}",
            smoke_delete,
            "\
diff --git a/activity-diff-live-check.md b/activity-diff-live-check.md
new file mode 100644
--- /dev/null
+++ b/activity-diff-live-check.md
@@ -0,0 +1,4 @@
+# Activity Diff Live Check
+
+- first event
+- second event
"
        );
        let third = tracker
            .delta(project_root, &[], &cumulative_after_modify)
            .unwrap();
        assert_eq!(
            third.files_changed,
            vec!["activity-diff-live-check.md".to_string()]
        );
        assert!(!third.unified_diff.contains("activity-diff-smoke.txt"));
        assert!(third
            .unified_diff
            .contains("--- a/activity-diff-live-check.md"));
        assert!(third.unified_diff.contains("+- second event"));
        assert!(!third.unified_diff.contains("+@"));
    }

    #[test]
    fn extract_json_from_json_fence() {
        let text = r#"Here is the command:
```json
{"commands": [{"function": "execAsAgent", "nonce": 1}]}
```
Done."#;
        let json = extract_json(text).unwrap();
        assert!(json.starts_with('{'));
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(parsed["commands"].is_array());
    }

    #[test]
    fn extract_json_from_generic_fence() {
        let text = r#"Result:
```
{"commands": []}
```"#;
        let json = extract_json(text).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(parsed["commands"].is_array());
    }

    #[test]
    fn extract_json_bare() {
        let text = r#"I'll run this: {"commands": [{"function": "inspectPath", "nonce": 1, "path": "/tmp"}]} end"#;
        let json = extract_json(text).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed["commands"][0]["function"], "inspectPath");
    }

    #[test]
    fn extract_json_no_json() {
        let text = "This is just plain text with no JSON.";
        assert!(extract_json(text).is_none());
    }

    #[test]
    fn extract_json_invalid_bare_json() {
        let text = "Some text with {broken json} here";
        assert!(extract_json(text).is_none());
    }

    #[test]
    fn extract_json_nested_braces() {
        let text = r#"```json
{"commands": [{"function": "execAsAgent", "command": "echo {hello}", "nonce": 1}]}
```"#;
        let json = extract_json(text).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed["commands"][0]["command"], "echo {hello}");
    }

    #[test]
    fn extract_json_prefers_json_fence() {
        let text = r#"```json
{"source": "json_fence"}
```
Also: {"source": "bare"}"#;
        let json = extract_json(text).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed["source"], "json_fence");
    }

    #[test]
    fn extract_json_empty_fence() {
        let text = "```json\n```";
        // Empty fence - no JSON starting with {
        assert!(extract_json(text).is_none());
    }

    #[test]
    fn extract_json_fence_with_whitespace() {
        let text = "```json\n  {\"key\": \"value\"}  \n```";
        let json = extract_json(text).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn parse_brief_found() {
        let text =
            "I did a bunch of work.\n\nBRIEF: Implemented the login feature and added tests.";
        let (brief, explicit) = parse_brief(text);
        assert_eq!(brief, "Implemented the login feature and added tests.");
        assert!(explicit);
    }

    #[test]
    fn parse_brief_not_found_uses_fallback() {
        let text = "I did a bunch of work. No brief marker here.";
        let (brief, explicit) = parse_brief(text);
        assert_eq!(brief, "I did a bunch of work. No brief marker here.");
        assert!(!explicit);
    }

    #[test]
    fn parse_brief_empty_value_uses_fallback() {
        let text = "Some output\nBRIEF:   \nMore text";
        let (brief, explicit) = parse_brief(text);
        assert_eq!(brief, "Some output");
        assert!(!explicit);
    }

    #[test]
    fn parse_brief_last_occurrence() {
        let text = "BRIEF: first\nsome text\nBRIEF: second and final";
        let (brief, explicit) = parse_brief(text);
        assert_eq!(brief, "second and final");
        assert!(explicit);
    }

    #[test]
    fn parse_brief_fallback_skips_headers() {
        let text = "# Summary\n\nThis is the main finding. It was significant.";
        let (brief, explicit) = parse_brief(text);
        assert_eq!(brief, "This is the main finding. It was significant.");
        assert!(!explicit);
    }

    #[test]
    fn parse_brief_fallback_empty_text() {
        let (brief, explicit) = parse_brief("");
        assert_eq!(brief, "Task completed.");
        assert!(!explicit);
    }

    #[test]
    fn apply_context_directives_drop_turns() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string());
        conv.add_assistant("a1".to_string());
        conv.add_user("u2".to_string());
        conv.add_assistant("a2".to_string());
        conv.add_user("u3".to_string());
        conv.add_assistant("a3".to_string());

        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}],"context":{"drop_turns":[1,2]}}"#;
        let (result, had_context) = apply_context_directives(json, &mut conv);

        // Messages 1,2 dropped (u1, a1)
        assert_eq!(conv.len(), 5);
        assert!(had_context);
        // context field stripped
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed.get("context").is_none());
        assert!(parsed.get("commands").is_some());
    }

    #[test]
    fn apply_context_directives_summarize() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string());
        conv.add_assistant("a1".to_string());
        conv.add_user("u2".to_string());
        conv.add_assistant("a2".to_string());
        conv.add_user("u3".to_string());
        conv.add_assistant("a3".to_string());

        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}],"context":{"summarize":{"turns":[1,2,3,4],"summary":"Setup phase"}}}"#;
        let (result, had_context) = apply_context_directives(json, &mut conv);

        assert_eq!(conv.len(), 4); // sys + summary + u3 + a3
        assert!(conv.messages()[1].content.contains("Setup phase"));
        assert!(had_context);
        assert!(!result.is_empty());
    }

    #[test]
    fn apply_context_directives_context_only() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string());
        conv.add_assistant("a1".to_string());
        conv.add_user("u2".to_string());
        conv.add_assistant("a2".to_string());
        conv.add_user("u3".to_string());
        conv.add_assistant("a3".to_string());

        let json = r#"{"commands":[],"context":{"drop_turns":[1,2]}}"#;
        let (result, had_context) = apply_context_directives(json, &mut conv);
        assert!(result.is_empty()); // no commands
        assert!(had_context); // but context was applied
    }

    #[test]
    fn apply_context_directives_no_context() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string());
        conv.add_assistant("a1".to_string());

        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}]}"#;
        let (result, had_context) = apply_context_directives(json, &mut conv);
        assert_eq!(conv.len(), 3); // unchanged
        assert!(!had_context);
        assert!(!result.is_empty());
    }

    #[test]
    fn apply_context_directives_empty_commands_no_context() {
        let mut conv = Conversation::new("sys".to_string(), 128_000);
        conv.add_user("u1".to_string());
        conv.add_assistant("a1".to_string());

        let json = r#"{"commands":[]}"#;
        let (result, had_context) = apply_context_directives(json, &mut conv);
        assert!(result.is_empty()); // no commands
        assert!(!had_context); // no context directives — signals task complete
    }

    #[test]
    fn done_signal_detected() {
        let json = r#"{"commands":[],"done":true,"message":"All tasks completed"}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(parsed
            .get("done")
            .and_then(|d| d.as_bool())
            .unwrap_or(false));
        assert_eq!(
            parsed.get("message").and_then(|m| m.as_str()),
            Some("All tasks completed")
        );
    }

    #[test]
    fn done_signal_without_message() {
        let json = r#"{"commands":[],"done":true}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(parsed
            .get("done")
            .and_then(|d| d.as_bool())
            .unwrap_or(false));
        assert!(parsed.get("message").and_then(|m| m.as_str()).is_none());
    }

    #[test]
    fn no_done_signal_continues() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}]}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(!parsed
            .get("done")
            .and_then(|d| d.as_bool())
            .unwrap_or(false));
    }

    #[test]
    fn inject_project_context_adds_memory_file() {
        let root = std::path::PathBuf::from("/tmp/proj");
        let project = Project {
            root: root.clone(),
            config: project::ProjectConfig::default(),
        };
        let input = r#"{"commands":[{"function":"storeMemory","nonce":1,"memory_key":"test","memory_summary":"hello"}]}"#;
        let result = inject_project_context(input, &project);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        // Build the expected path the same platform-aware way production does
        // (via `PathBuf::join`) instead of hardcoding '/'-joined POSIX text,
        // so the assertion holds on Windows (separator '\\') too.
        let expected = root
            .join(".intendant")
            .join("memory.json")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            parsed["commands"][0]["memory_file"].as_str().unwrap(),
            expected
        );
    }

    #[test]
    fn inject_project_context_preserves_existing() {
        let project = Project {
            root: std::path::PathBuf::from("/tmp/proj"),
            config: project::ProjectConfig::default(),
        };
        let input = r#"{"commands":[{"function":"storeMemory","nonce":1,"memory_file":"/custom/path.json"}]}"#;
        let result = inject_project_context(input, &project);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            parsed["commands"][0]["memory_file"].as_str().unwrap(),
            "/custom/path.json"
        );
    }

    #[test]
    fn inject_project_context_ignores_unrelated() {
        let project = Project {
            root: std::path::PathBuf::from("/tmp/proj"),
            config: project::ProjectConfig::default(),
        };
        let input = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}]}"#;
        let result = inject_project_context(input, &project);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["commands"][0].get("memory_file").is_none());
        assert!(parsed["commands"][0].get("project_dir").is_none());
    }

    #[test]
    fn budget_constants_are_sane() {
        assert!(SAFETY_CAP > 0);
        assert!(MIN_BUDGET_TOKENS > 0);
        assert!(BUDGET_WARNING_THRESHOLD > 0.0 && BUDGET_WARNING_THRESHOLD < 1.0);
    }

    #[test]
    fn is_simple_task_short() {
        assert!(is_simple_task("list files in /tmp"));
        assert!(is_simple_task("what is 2+2"));
        assert!(is_simple_task("echo hello"));
    }

    #[test]
    fn is_simple_task_complex_keywords() {
        assert!(!is_simple_task(
            "research the database schema and document findings"
        ));
        assert!(!is_simple_task("implement a new authentication system"));
        assert!(!is_simple_task("refactor the payment module"));
        assert!(!is_simple_task("build and deploy the application"));
        assert!(!is_simple_task("investigate why the tests are failing"));
    }

    #[test]
    fn is_simple_task_long() {
        let long_task = "x".repeat(150);
        assert!(!is_simple_task(&long_task));
    }

    #[test]
    fn is_simple_task_multiline() {
        assert!(!is_simple_task("line1\nline2\nline3\nline4"));
    }

    fn cli_flags_for_tests() -> CliFlags {
        CliFlags {
            task: None,
            task_file: None,
            provider: None,
            model: None,
            verbose: false,
            no_tui: false,
            mcp: false,
            autonomy: AutonomyLevel::Medium,
            log_file: None,
            continue_last: false,
            resume_id: None,
            control_socket: false,
            json_output: false,
            sandbox: false,
            direct: false,
            no_presence: false,
            web: false,
            web_port: web_gateway::DEFAULT_PORT,
            web_bind: None,
            owner: None,
            no_tls: false,
            allow_public_plaintext: false,
            tls: false,
            tls_cert: None,
            tls_key: None,
            mtls: false,
            mtls_ca: None,
            transcription: false,
            record_displays: Vec::new(),
            agent_backend: None,
            no_web: false,
            advertise_urls: Vec::new(),
        }
    }

    #[test]
    fn idle_web_defaults_to_daemon_without_no_tui() {
        let flags = cli_flags_for_tests();
        assert!(should_start_idle_web_daemon(true, &flags));
    }

    #[test]
    fn idle_web_daemon_requires_web_and_no_task() {
        let mut flags = cli_flags_for_tests();
        assert!(!should_start_idle_web_daemon(false, &flags));

        flags.task = Some("do the thing".to_string());
        assert!(!should_start_idle_web_daemon(true, &flags));

        flags.task = None;
        flags.task_file = Some("/tmp/intendant-task.txt".to_string());
        assert!(!should_start_idle_web_daemon(true, &flags));
    }

    #[test]
    fn task_file_is_trimmed_for_initial_task() {
        let dir = tempfile::tempdir().unwrap();
        let task_path = dir.path().join("task.txt");
        std::fs::write(&task_path, "long managed prompt\n").unwrap();

        let mut flags = cli_flags_for_tests();
        flags.task_file = Some(task_path.to_string_lossy().into_owned());

        let task = get_task_from_flags_or_env(&flags).unwrap();
        assert_eq!(task, "long managed prompt");
    }

    #[test]
    fn mcp_task_file_is_initial_task() {
        let dir = tempfile::tempdir().unwrap();
        let task_path = dir.path().join("task.txt");
        std::fs::write(&task_path, "serve this task over mcp\n").unwrap();

        let mut flags = cli_flags_for_tests();
        flags.mcp = true;
        flags.task_file = Some(task_path.to_string_lossy().into_owned());

        let task = resolve_initial_task_for_startup(&flags, false, false).unwrap();
        assert_eq!(task.as_deref(), Some("serve this task over mcp"));
    }

    #[test]
    fn web_task_file_is_initial_task_even_when_tui_mode_is_available() {
        let dir = tempfile::tempdir().unwrap();
        let task_path = dir.path().join("task.txt");
        std::fs::write(&task_path, "resume managed harness\n").unwrap();

        let mut flags = cli_flags_for_tests();
        flags.task_file = Some(task_path.to_string_lossy().into_owned());
        flags.web = true;
        flags.no_tui = true;
        flags.no_presence = true;
        flags.agent_backend = Some(external_agent::AgentBackend::Codex);
        flags.resume_id = Some("6036429e-54f9-4f93-b74d-04c060c79054".to_string());

        let web_daemon_requested = should_start_idle_web_daemon(true, &flags);
        let use_tui = !web_daemon_requested;
        let task = resolve_initial_task_for_startup(&flags, web_daemon_requested, use_tui).unwrap();
        assert_eq!(task.as_deref(), Some("resume managed harness"));
    }

    #[test]
    fn external_agent_startup_resume_uses_persisted_wrapper_backend_session() {
        let home = tempfile::tempdir().unwrap();
        let wrapper_session_id = "6036429e-54f9-4f93-b74d-04c060c79054";
        let backend_session_id = "019ea9da-d0d6-7800-acae-a16366f02a92";
        let wrapper_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join(wrapper_session_id);
        {
            let mut log = session_log::SessionLog::open(wrapper_dir).unwrap();
            log.write_meta(Some(home.path()), Some("old task"));
            log.session_identity(wrapper_session_id, "codex", backend_session_id);
        }

        let mut flags = cli_flags_for_tests();
        flags.agent_backend = Some(external_agent::AgentBackend::Codex);
        flags.resume_id = Some(wrapper_session_id.to_string());
        flags.task_file = Some("/tmp/station-managed-resume-task.txt".to_string());

        let resume_session = external_resume_session_for_startup_in_home(
            home.path(),
            flags.agent_backend.as_ref(),
            &flags,
            Some(wrapper_session_id),
        );

        assert_eq!(resume_session.as_deref(), Some(backend_session_id));
    }

    fn default_codex_project() -> Project {
        // A root without intendant.toml loads pure defaults — the stand-in for
        // the "global/TOML" config a CLI startup builds before any resume.
        let root = tempfile::tempdir().unwrap();
        let project = Project::from_root(root.path().to_path_buf()).unwrap();
        assert_eq!(project.config.agent.codex.managed_context, "vanilla");
        project
    }

    #[test]
    fn startup_resume_applies_persisted_session_config_over_global_default() {
        let home = tempfile::tempdir().unwrap();
        let mut project = default_codex_project();
        let mut persisted = session_config::from_wire(
            Some("codex"),
            Some("/opt/codex-fork/codex"),
            Some("danger-full-access"),
            Some("never"),
            Some("managed"),
            Some("exact"),
            Some("priority"),
        );
        persisted.codex_home = Some("/home/user/.codex-managed".to_string());
        session_config::write_external_overlay(home.path(), "codex", "backend-thread", &persisted)
            .unwrap();

        let overrides = apply_startup_external_resume_config_in_home(
            home.path(),
            &external_agent::AgentBackend::Codex,
            &mut project,
            Some("wrapper-session"),
            Some("backend-thread"),
            session_config::SessionAgentConfig::default(),
        )
        .expect("persisted overlay should produce startup overrides");

        let codex = &project.config.agent.codex;
        assert_eq!(codex.managed_context, "managed");
        assert_eq!(codex.command, "/opt/codex-fork/codex");
        assert_eq!(codex.sandbox, "danger-full-access");
        assert_eq!(codex.approval_policy, "never");
        assert_eq!(codex.context_archive, "exact");
        assert_eq!(overrides.codex_service_tier.as_deref(), Some("priority"));
        assert_eq!(
            overrides.codex_home.as_deref(),
            Some("/home/user/.codex-managed")
        );
    }

    #[test]
    fn startup_resume_overlay_is_found_by_wrapper_session_id_too() {
        let home = tempfile::tempdir().unwrap();
        let mut project = default_codex_project();
        let persisted =
            session_config::from_wire(Some("codex"), None, None, None, Some("managed"), None, None);
        // Overlay keyed by the wrapper/intendant session id, not the backend
        // thread id — `load_for_resume` must check both.
        session_config::write_external_overlay(home.path(), "codex", "wrapper-session", &persisted)
            .unwrap();

        apply_startup_external_resume_config_in_home(
            home.path(),
            &external_agent::AgentBackend::Codex,
            &mut project,
            Some("wrapper-session"),
            Some("backend-thread"),
            session_config::SessionAgentConfig::default(),
        )
        .expect("overlay keyed by wrapper id should produce startup overrides");

        assert_eq!(project.config.agent.codex.managed_context, "managed");
    }

    #[test]
    fn startup_resume_explicit_overrides_win_over_persisted_config() {
        let home = tempfile::tempdir().unwrap();
        let mut project = default_codex_project();
        let persisted = session_config::from_wire(
            Some("codex"),
            Some("/opt/codex-fork/codex"),
            None,
            None,
            Some("managed"),
            None,
            None,
        );
        session_config::write_external_overlay(home.path(), "codex", "backend-thread", &persisted)
            .unwrap();

        // An explicit (e.g. future CLI-flag) override must keep winning over
        // the persisted per-session config, like the supervisor's wire fields.
        let explicit = session_config::from_wire(
            Some("codex"),
            Some("/usr/local/bin/codex"),
            None,
            None,
            Some("vanilla"),
            None,
            None,
        );
        apply_startup_external_resume_config_in_home(
            home.path(),
            &external_agent::AgentBackend::Codex,
            &mut project,
            Some("wrapper-session"),
            Some("backend-thread"),
            explicit,
        )
        .expect("explicit overrides should produce startup overrides");

        assert_eq!(project.config.agent.codex.managed_context, "vanilla");
        assert_eq!(project.config.agent.codex.command, "/usr/local/bin/codex");
    }

    #[test]
    fn startup_resume_without_persisted_config_keeps_global_config() {
        let home = tempfile::tempdir().unwrap();
        let mut project = default_codex_project();
        let default_command = project.config.agent.codex.command.clone();

        let overrides = apply_startup_external_resume_config_in_home(
            home.path(),
            &external_agent::AgentBackend::Codex,
            &mut project,
            Some("wrapper-session"),
            Some("backend-thread"),
            session_config::SessionAgentConfig::default(),
        );

        assert!(overrides.is_none(), "no overlay should mean no overrides");
        assert_eq!(project.config.agent.codex.managed_context, "vanilla");
        assert_eq!(project.config.agent.codex.command, default_command);
    }

    #[test]
    fn startup_without_resume_token_never_loads_persisted_config() {
        let home = tempfile::tempdir().unwrap();
        let mut project = default_codex_project();
        let persisted =
            session_config::from_wire(Some("codex"), None, None, None, Some("managed"), None, None);
        session_config::write_external_overlay(home.path(), "codex", "wrapper-session", &persisted)
            .unwrap();

        let overrides = apply_startup_external_resume_config_in_home(
            home.path(),
            &external_agent::AgentBackend::Codex,
            &mut project,
            Some("wrapper-session"),
            None,
            session_config::SessionAgentConfig::default(),
        );

        assert!(overrides.is_none(), "fresh startups must stay untouched");
        assert_eq!(project.config.agent.codex.managed_context, "vanilla");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn black_frame_detector_rejects_all_zero_rgb() {
        let frame = display::Frame {
            data: vec![0; 4 * 4 * 4],
            format: display::FrameFormat::Bgra,
            width: 4,
            height: 4,
            stride: 16,
            timestamp: std::time::Instant::now(),
            dirty_rects: None,
        };

        assert!(!frame_has_visible_rgb(&frame));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn black_frame_detector_accepts_visible_rgb() {
        let mut data = vec![0; 4 * 4 * 4];
        data[10] = 64;
        let frame = display::Frame {
            data,
            format: display::FrameFormat::Bgra,
            width: 4,
            height: 4,
            stride: 16,
            timestamp: std::time::Instant::now(),
            dirty_rects: None,
        };

        assert!(frame_has_visible_rgb(&frame));
    }

    struct ActiveDisplayBackend {
        width: u32,
        height: u32,
    }

    #[async_trait::async_trait]
    impl display::DisplayBackend for ActiveDisplayBackend {
        async fn start_capture(
            &self,
            _fps: u32,
        ) -> Result<tokio::sync::mpsc::Receiver<display::Frame>, error::CallerError> {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(rx)
        }

        async fn stop_capture(&self) {}

        async fn inject_input(
            &self,
            _event: display::InputEvent,
        ) -> Result<(), error::CallerError> {
            Ok(())
        }

        fn resolution(&self) -> (u32, u32) {
            (self.width, self.height)
        }

        fn kind(&self) -> &'static str {
            "test"
        }
    }

    #[test]
    fn activate_user_display_skips_activation_when_capture_already_active() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let bus = EventBus::new();
            let mut rx = bus.subscribe();
            let backend = std::sync::Arc::new(ActiveDisplayBackend {
                width: 1920,
                height: 1080,
            });
            let session = std::sync::Arc::new(display::DisplaySession::new(0, backend));
            let mut registry = display::SessionRegistry::new();
            registry.insert(0, session);
            let registry = std::sync::Arc::new(tokio::sync::RwLock::new(registry));

            activate_user_display(&bus, &registry, None, 0).await;

            match tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await {
                Ok(Ok(AppEvent::DisplayReady {
                    display_id,
                    width,
                    height,
                })) => {
                    assert_eq!(display_id, 0);
                    assert_eq!((width, height), (1920, 1080));
                }
                other => panic!("expected DisplayReady for active capture, got {other:?}"),
            }
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv())
                    .await
                    .is_err(),
                "already-active capture should not emit a portal-pending event"
            );
        });
    }

    #[test]
    fn parse_cli_flags_empty() {
        // Can't easily test parse_cli_flags since it reads env::args(),
        // but we can test the struct defaults
        let flags = CliFlags {
            task: None,
            task_file: None,
            provider: None,
            model: None,
            verbose: false,
            no_tui: false,
            mcp: false,
            autonomy: AutonomyLevel::Medium,
            log_file: None,
            continue_last: false,
            resume_id: None,
            control_socket: false,
            json_output: false,
            sandbox: false,
            direct: false,
            no_presence: false,
            web: false,
            web_port: web_gateway::DEFAULT_PORT,
            web_bind: None,
            owner: None,
            no_tls: false,
            allow_public_plaintext: false,
            tls: false,
            tls_cert: None,
            tls_key: None,
            mtls: false,
            mtls_ca: None,
            transcription: false,
            record_displays: Vec::new(),

            agent_backend: None,

            no_web: false,

            advertise_urls: Vec::new(),
        };
        assert!(!flags.verbose);
        assert!(!flags.no_tui);
        assert!(!flags.mcp);
        assert!(!flags.continue_last);
        assert!(flags.resume_id.is_none());
        assert!(!flags.sandbox);
        assert!(!flags.json_output);
        assert!(!flags.direct);
        assert!(!flags.no_presence);
        assert!(!flags.web);
        assert!(!flags.no_web);
        assert!(!flags.transcription);
        assert_eq!(flags.web_port, 8765);
        assert_eq!(flags.autonomy, AutonomyLevel::Medium);
    }

    #[test]
    fn cli_web_flag() {
        let flags = CliFlags {
            task: None,
            task_file: None,
            provider: None,
            model: None,
            verbose: false,
            no_tui: false,
            mcp: false,
            autonomy: AutonomyLevel::Medium,
            log_file: None,
            continue_last: false,
            resume_id: None,
            control_socket: false,
            json_output: false,
            sandbox: false,
            direct: false,
            no_presence: false,
            web: true,
            web_port: web_gateway::DEFAULT_PORT,
            web_bind: None,
            owner: None,
            no_tls: false,
            allow_public_plaintext: false,
            tls: false,
            tls_cert: None,
            tls_key: None,
            mtls: false,
            mtls_ca: None,
            transcription: false,
            record_displays: Vec::new(),

            agent_backend: None,

            no_web: false,

            advertise_urls: Vec::new(),
        };
        assert!(flags.web);
        assert_eq!(flags.web_port, web_gateway::DEFAULT_PORT);
    }

    #[test]
    fn cli_web_with_port() {
        let flags = CliFlags {
            task: None,
            task_file: None,
            provider: None,
            model: None,
            verbose: false,
            no_tui: false,
            mcp: false,
            autonomy: AutonomyLevel::Medium,
            log_file: None,
            continue_last: false,
            resume_id: None,
            control_socket: false,
            json_output: false,
            sandbox: false,
            direct: false,
            no_presence: false,
            web: true,
            web_port: 9000,
            web_bind: None,
            owner: None,
            no_tls: false,
            allow_public_plaintext: false,
            tls: false,
            tls_cert: None,
            tls_key: None,
            mtls: false,
            mtls_ca: None,
            transcription: false,
            record_displays: Vec::new(),

            agent_backend: None,

            no_web: false,

            advertise_urls: Vec::new(),
        };
        assert!(flags.web);
        assert_eq!(flags.web_port, 9000);
    }

    #[test]
    fn web_tls_policy_defaults_to_mtls() {
        let flags = cli_flags_for_tests();
        let tls_cfg = project::ServerTlsConfig::default();
        let mtls_cfg = project::ServerMutualTlsConfig::default();
        assert!(web_default_mtls_enabled(&flags, &tls_cfg));
        assert!(web_mtls_enabled(&flags, &tls_cfg, &mtls_cfg));
    }

    #[test]
    fn web_tls_policy_tls_only_disables_default_mtls() {
        let mut flags = cli_flags_for_tests();
        flags.tls = true;
        let tls_cfg = project::ServerTlsConfig::default();
        let mtls_cfg = project::ServerMutualTlsConfig::default();
        assert!(!web_default_mtls_enabled(&flags, &tls_cfg));
        assert!(!web_mtls_enabled(&flags, &tls_cfg, &mtls_cfg));
    }

    #[test]
    fn web_tls_policy_no_tls_is_plaintext_escape() {
        let mut flags = cli_flags_for_tests();
        flags.no_tls = true;
        let tls_cfg = project::ServerTlsConfig::default();
        let mtls_cfg = project::ServerMutualTlsConfig::default();
        assert!(!web_default_mtls_enabled(&flags, &tls_cfg));
        assert!(!web_mtls_enabled(&flags, &tls_cfg, &mtls_cfg));
    }

    #[test]
    fn web_tls_policy_configured_tls_is_tls_only() {
        let flags = cli_flags_for_tests();
        let tls_cfg = project::ServerTlsConfig {
            enabled: true,
            ..Default::default()
        };
        let mtls_cfg = project::ServerMutualTlsConfig::default();
        assert!(!web_default_mtls_enabled(&flags, &tls_cfg));
        assert!(!web_mtls_enabled(&flags, &tls_cfg, &mtls_cfg));
    }

    #[test]
    fn no_tls_rejects_tls_flags() {
        let mut flags = cli_flags_for_tests();
        flags.no_tls = true;
        flags.tls = true;
        let err = validate_tls_cli_flags(&flags).unwrap_err();
        assert!(err.to_string().contains("--no-tls"), "err: {err}");
    }

    #[test]
    fn effective_web_bind_cli_overrides_config() {
        let mut flags = cli_flags_for_tests();
        flags.web_bind = Some("127.0.0.1".parse().unwrap());
        let server_cfg = project::ServerConfig {
            bind: Some("10.0.0.2".parse().unwrap()),
            ..Default::default()
        };
        assert_eq!(
            effective_web_bind_ip(&flags, &server_cfg),
            Some("127.0.0.1".parse().unwrap())
        );
    }

    #[test]
    fn no_tls_wildcard_rejects_public_interface_without_override() {
        let mut flags = cli_flags_for_tests();
        flags.no_tls = true;
        let public_addrs = vec!["8.8.8.8".parse().unwrap()];
        let err =
            validate_plaintext_web_bind_with_public_addrs(&flags, None, &public_addrs).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--no-tls"), "msg: {msg}");
        assert!(msg.contains("--bind 127.0.0.1"), "msg: {msg}");
    }

    #[test]
    fn no_tls_wildcard_allows_private_interfaces() {
        let mut flags = cli_flags_for_tests();
        flags.no_tls = true;
        assert!(validate_plaintext_web_bind_with_public_addrs(&flags, None, &[]).is_ok());
    }

    #[test]
    fn no_tls_specific_bind_allows_public_interface() {
        let mut flags = cli_flags_for_tests();
        flags.no_tls = true;
        let public_addrs = vec!["8.8.8.8".parse().unwrap()];
        assert!(validate_plaintext_web_bind_with_public_addrs(
            &flags,
            Some("127.0.0.1".parse().unwrap()),
            &public_addrs,
        )
        .is_ok());
    }

    #[test]
    fn no_tls_wildcard_public_override_is_explicit() {
        let mut flags = cli_flags_for_tests();
        flags.no_tls = true;
        flags.allow_public_plaintext = true;
        let public_addrs = vec!["8.8.8.8".parse().unwrap()];
        assert!(validate_plaintext_web_bind_with_public_addrs(&flags, None, &public_addrs).is_ok());
    }

    #[tokio::test]
    async fn user_approval_side_effects_apply_on_every_surface() {
        let autonomy = autonomy::shared_autonomy(AutonomyState::default());
        let bus = EventBus::new();
        let mut events = bus.subscribe();

        // Plain approval records the command for dedup; a DisplayControl
        // approval grants the user display session-wide. The TUI/MCP
        // registry path used to skip both, so approving there left the
        // action failing its grant check afterwards.
        apply_user_approval(
            event::ApprovalResponse::Approve,
            autonomy::ActionCategory::DisplayControl,
            "cu: click",
            &autonomy,
            &bus,
        )
        .await;
        {
            let state = autonomy.read().await;
            assert!(state.was_command_approved("cu: click"));
            assert!(state.user_display_granted);
        }
        let mut saw_grant = false;
        while let Ok(event) = events.try_recv() {
            if matches!(event, AppEvent::UserDisplayGranted { .. }) {
                saw_grant = true;
            }
        }
        assert!(saw_grant, "UserDisplayGranted must be announced");

        // Approve-all escalates autonomy for the rest of the session.
        apply_user_approval(
            event::ApprovalResponse::ApproveAll,
            autonomy::ActionCategory::CommandExec,
            "rm -rf target",
            &autonomy,
            &bus,
        )
        .await;
        assert_eq!(autonomy.read().await.level, AutonomyLevel::Full);

        // Deny and skip carry no side effects.
        apply_user_approval(
            event::ApprovalResponse::Deny,
            autonomy::ActionCategory::CommandExec,
            "never ran",
            &autonomy,
            &bus,
        )
        .await;
        assert!(!autonomy.read().await.was_command_approved("never ran"));
    }

    #[test]
    fn format_command_preview_exec() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls -la /tmp"}]}"#;
        let preview = format_command_preview(json);
        assert!(preview.contains("exec: ls -la /tmp"));
    }

    #[test]
    fn format_command_preview_write_file() {
        let json = r#"{"commands":[{"function":"writeFile","nonce":1,"file_path":"/home/user/test.rs","content":"fn main(){}"}]}"#;
        let preview = format_command_preview(json);
        assert!(preview.contains("writeFile: /home/user/test.rs"));
    }

    #[test]
    fn format_command_preview_multiple() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"cargo build"},{"function":"inspectPath","nonce":2,"path":"/tmp"}]}"#;
        let preview = format_command_preview(json);
        assert!(preview.contains("exec: cargo build"));
        assert!(preview.contains("inspect: /tmp"));
        assert!(preview.contains(" | "));
    }

    #[test]
    fn format_command_preview_inspect() {
        let json = r#"{"commands":[{"function":"inspectPath","nonce":1,"path":"/tmp/dir"}]}"#;
        let preview = format_command_preview(json);
        assert!(preview.contains("inspect: /tmp/dir"));
    }

    #[test]
    fn format_command_preview_browse() {
        let json = r#"{"commands":[{"function":"browse","nonce":1,"url":"https://example.com"}]}"#;
        let preview = format_command_preview(json);
        assert!(preview.contains("browse: https://example.com"));
    }

    #[test]
    fn format_command_preview_invalid_json() {
        let json = "not json at all";
        let preview = format_command_preview(json);
        assert_eq!(preview, "not json at all");
    }

    #[test]
    fn has_ask_human_command_true() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1},{"function":"askHuman","nonce":2}]}"#;
        assert!(has_ask_human_command(json));
    }

    #[test]
    fn has_ask_human_command_false() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1}]}"#;
        assert!(!has_ask_human_command(json));
    }

    #[test]
    fn has_ask_human_command_invalid_json() {
        assert!(!has_ask_human_command("not json"));
    }

    #[test]
    fn has_capture_screen_command_true() {
        let json = r#"{"commands":[{"function":"captureScreen","nonce":1}]}"#;
        assert!(has_capture_screen_command(json));
    }

    #[test]
    fn has_capture_screen_command_false() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1}]}"#;
        assert!(!has_capture_screen_command(json));
    }

    #[test]
    fn has_capture_screen_command_mixed_batch() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1},{"function":"captureScreen","nonce":2}]}"#;
        assert!(has_capture_screen_command(json));
    }

    #[test]
    fn has_capture_screen_command_invalid_json() {
        assert!(!has_capture_screen_command("not json"));
    }

    #[test]
    fn encode_screenshot_valid() {
        let dir = tempfile::tempdir().unwrap();
        let img_path = dir.path().join("test.png");
        std::fs::write(&img_path, b"\x89PNG\r\n\x1a\n").unwrap();
        let json = serde_json::json!({
            "success": true,
            "screenshot_path": img_path.to_str().unwrap(),
        });
        let result = encode_screenshot(&json.to_string());
        assert!(result.is_some());
        let images = result.unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, "image/png");
        assert!(!images[0].data.is_empty());
    }

    #[test]
    fn encode_screenshot_missing_file() {
        let json = r#"{"success":true,"screenshot_path":"/tmp/nonexistent_screenshot_12345.png"}"#;
        assert!(encode_screenshot(json).is_none());
    }

    #[test]
    fn encode_screenshot_success_false() {
        let json = r#"{"success":false,"screenshot_path":"/tmp/whatever.png"}"#;
        assert!(encode_screenshot(json).is_none());
    }

    #[test]
    fn encode_screenshot_invalid_json() {
        assert!(encode_screenshot("not json").is_none());
    }

    #[test]
    fn encode_screenshot_missing_path_field() {
        let json = r#"{"success":true}"#;
        assert!(encode_screenshot(json).is_none());
    }

    #[test]
    fn has_exec_command_true() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}]}"#;
        assert!(has_exec_command(json));
    }

    #[test]
    fn has_exec_command_pty() {
        let json = r#"{"commands":[{"function":"execPty","nonce":1,"command":"ls"}]}"#;
        assert!(has_exec_command(json));
    }

    #[test]
    fn has_exec_command_false_for_non_exec() {
        let json = r#"{"commands":[{"function":"captureScreen","nonce":1}]}"#;
        assert!(!has_exec_command(json));
    }

    #[test]
    fn has_exec_command_invalid_json() {
        assert!(!has_exec_command("not json"));
    }

    // --- assemble_batch_from_tool_calls tests ---

    #[test]
    fn assemble_batch_single_exec() {
        let calls = vec![provider::ToolCall {
            id: "call_1".to_string(),
            call_id: "call_1".to_string(),
            name: "exec_command".to_string(),
            arguments: r#"{"nonce":1,"command":"ls -la"}"#.to_string(),
        }];
        let result = assemble_batch_from_tool_calls(&calls);
        assert!(!result.is_done);
        assert!(result.context_directives.is_none());
        assert!(result.agent_input_json.is_some());

        let input: serde_json::Value =
            serde_json::from_str(result.agent_input_json.as_ref().unwrap()).unwrap();
        assert_eq!(input["commands"][0]["function"], "execAsAgent");
        assert_eq!(input["commands"][0]["command"], "ls -la");
        assert_eq!(input["commands"][0]["nonce"], 1);
        assert_eq!(result.nonce_to_call_id.get(&1), Some(&"call_1".to_string()));
    }

    #[test]
    fn assemble_batch_signal_done() {
        let calls = vec![provider::ToolCall {
            id: "call_1".to_string(),
            call_id: "call_1".to_string(),
            name: "signal_done".to_string(),
            arguments: r#"{"message":"All tasks completed"}"#.to_string(),
        }];
        let result = assemble_batch_from_tool_calls(&calls);
        assert!(result.is_done);
        assert_eq!(result.done_message.as_deref(), Some("All tasks completed"));
        assert!(result.agent_input_json.is_none());
    }

    #[test]
    fn assemble_batch_signal_done_no_message() {
        let calls = vec![provider::ToolCall {
            id: "call_1".to_string(),
            call_id: "call_1".to_string(),
            name: "signal_done".to_string(),
            arguments: r#"{}"#.to_string(),
        }];
        let result = assemble_batch_from_tool_calls(&calls);
        assert!(result.is_done);
        assert!(result.done_message.is_none());
    }

    #[test]
    fn assemble_batch_manage_context() {
        let calls = vec![provider::ToolCall {
            id: "call_1".to_string(),
            call_id: "call_1".to_string(),
            name: "manage_context".to_string(),
            arguments: r#"{"drop_turns":[1,2]}"#.to_string(),
        }];
        let result = assemble_batch_from_tool_calls(&calls);
        assert!(!result.is_done);
        assert!(result.agent_input_json.is_none());
        assert!(result.context_directives.is_some());
        let ctx = result.context_directives.unwrap();
        assert_eq!(ctx["drop_turns"][0], 1);
        assert_eq!(ctx["drop_turns"][1], 2);
    }

    #[test]
    fn assemble_batch_mixed_tools() {
        let calls = vec![
            provider::ToolCall {
                id: "call_1".to_string(),
                call_id: "call_1".to_string(),
                name: "exec_command".to_string(),
                arguments: r#"{"nonce":10,"command":"echo hello"}"#.to_string(),
            },
            provider::ToolCall {
                id: "call_2".to_string(),
                call_id: "call_2".to_string(),
                name: "inspect_path".to_string(),
                arguments: r#"{"nonce":11,"path":"/tmp"}"#.to_string(),
            },
            provider::ToolCall {
                id: "call_3".to_string(),
                call_id: "call_3".to_string(),
                name: "manage_context".to_string(),
                arguments: r#"{"drop_turns":[3]}"#.to_string(),
            },
        ];
        let result = assemble_batch_from_tool_calls(&calls);
        assert!(!result.is_done);
        assert!(result.context_directives.is_some());
        assert!(result.agent_input_json.is_some());

        let input: serde_json::Value =
            serde_json::from_str(result.agent_input_json.as_ref().unwrap()).unwrap();
        let commands = input["commands"].as_array().unwrap();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0]["function"], "execAsAgent");
        assert_eq!(commands[1]["function"], "inspectPath");
        assert_eq!(result.nonce_to_call_id.len(), 2);
        assert_eq!(result.call_id_names.len(), 3);
    }

    #[test]
    fn assemble_batch_unknown_tool_ignored() {
        let calls = vec![provider::ToolCall {
            id: "call_1".to_string(),
            call_id: "call_1".to_string(),
            name: "nonexistent_tool".to_string(),
            arguments: r#"{"nonce":1}"#.to_string(),
        }];
        let result = assemble_batch_from_tool_calls(&calls);
        assert!(result.agent_input_json.is_none());
    }

    #[test]
    fn assemble_batch_duplicate_nonce_emits_error() {
        let calls = vec![
            provider::ToolCall {
                id: "call_1".to_string(),
                call_id: "call_1".to_string(),
                name: "exec_command".to_string(),
                arguments: r#"{"nonce":1,"command":"echo a"}"#.to_string(),
            },
            provider::ToolCall {
                id: "call_2".to_string(),
                call_id: "call_2".to_string(),
                name: "inspect_path".to_string(),
                arguments: r#"{"nonce":1,"path":"/tmp"}"#.to_string(),
            },
        ];
        let result = assemble_batch_from_tool_calls(&calls);
        assert_eq!(result.precomputed_results.len(), 1);
        assert!(result.precomputed_results[0]
            .2
            .contains("duplicate nonce 1"));
    }

    #[test]
    fn assemble_batch_tool_name_mapping() {
        // Verify all tool names map correctly
        let tool_pairs = vec![
            ("exec_command", "execAsAgent"),
            ("capture_screen", "captureScreen"),
            ("inspect_path", "inspectPath"),
            ("edit_file", "editFile"),
            ("browse_url", "browse"),
            ("ask_human", "askHuman"),
            ("exec_pty", "execPty"),
            ("store_memory", "storeMemory"),
            ("recall_memory", "recallMemory"),
        ];
        for (tool_name, expected_func) in tool_pairs {
            let calls = vec![provider::ToolCall {
                id: "call_1".to_string(),
                call_id: "call_1".to_string(),
                name: tool_name.to_string(),
                arguments: r#"{"nonce":1,"command":"test","status_type":"stdout","path":"/tmp","file_path":"/tmp/f","operation":"write","url":"http://x","question":"?","memory_key":"k","memory_summary":"s","memory_query":"q"}"#.to_string(),
            }];
            let result = assemble_batch_from_tool_calls(&calls);
            let input: serde_json::Value =
                serde_json::from_str(result.agent_input_json.as_ref().unwrap()).unwrap();
            assert_eq!(
                input["commands"][0]["function"].as_str().unwrap(),
                expected_func,
                "Tool {} should map to function {}",
                tool_name,
                expected_func
            );
        }
    }

    // --- handle_shared_view_calls tests ---

    #[tokio::test]
    async fn shared_view_calls_validate_and_gate_user_session() {
        let tmp = tempfile::tempdir().unwrap();
        let session_log: SharedSessionLog = Arc::new(Mutex::new(
            session_log::SessionLog::open(tmp.path().to_path_buf()).unwrap(),
        ));
        let mut conversation = Conversation::new("system".to_string(), 100_000);
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let autonomy = autonomy::shared_autonomy(autonomy::AutonomyState::default());
        let mut counter = 0u64;

        let calls = vec![
            ("c1".to_string(), serde_json::json!({"action": "hide"})),
            // focus without a region must fail fast.
            ("c2".to_string(), serde_json::json!({"action": "focus"})),
            // user_session without the display grant is refused (explicit opt-in).
            (
                "c3".to_string(),
                serde_json::json!({"action": "show", "display_target": "user_session"}),
            ),
            ("c4".to_string(), serde_json::json!({"action": "bogus"})),
        ];
        handle_shared_view_calls(
            &calls,
            &mut conversation,
            &bus,
            &autonomy,
            None,
            Some("sess-1".to_string()),
            tmp.path(),
            &mut counter,
            &session_log,
        )
        .await;

        let results: Vec<_> = conversation
            .messages()
            .iter()
            .filter(|m| m.role == "tool")
            .collect();
        assert_eq!(results.len(), 4, "one result per call");
        assert!(
            results[0].content.contains("dismissed"),
            "{}",
            results[0].content
        );
        assert!(
            results[1].content.contains("requires a region"),
            "{}",
            results[1].content
        );
        assert!(
            results[2].content.contains("explicit opt-in"),
            "{}",
            results[2].content
        );
        assert!(
            results[3].content.contains("unknown shared_view action"),
            "{}",
            results[3].content
        );

        // Only the valid hide emitted a SharedView event; the gated and
        // invalid calls must not reach the dashboard.
        match rx.try_recv() {
            Ok(AppEvent::SharedView {
                action, session_id, ..
            }) => {
                assert_eq!(action, "hide");
                assert_eq!(session_id.as_deref(), Some("sess-1"));
            }
            other => panic!("expected SharedView hide event, got {other:?}"),
        }
        assert!(rx.try_recv().is_err(), "no further events expected");
    }

    // --- map_results_to_tool_responses tests ---

    #[test]
    fn map_results_single_exec() {
        let stdout = "{\"type\":\"status\",\"nonce\":1,\"status\":\"r\",\"pid\":0,\"exit_code\":0}\n{\"type\":\"status\",\"nonce\":1,\"status\":\"c\",\"pid\":1234,\"exit_code\":0}\n";
        let stderr = "";
        let mut nonce_map = std::collections::HashMap::new();
        nonce_map.insert(1u64, "call_1".to_string());
        let call_ids = vec![("call_1".to_string(), "exec_command".to_string())];

        let results = map_results_to_tool_responses(stdout, stderr, &nonce_map, &call_ids);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "call_1");
        assert!(results[0].2.contains("1c0"));
    }

    #[test]
    fn map_results_with_result_output() {
        let stdout = "{\"type\":\"status\",\"nonce\":5,\"status\":\"c\",\"pid\":0,\"exit_code\":0}\n{\"type\":\"result\",\"nonce\":5,\"data\":\"{\\\"content\\\":\\\"hello\\\",\\\"total_size\\\":5}\"}\n";
        let stderr = "";
        let mut nonce_map = std::collections::HashMap::new();
        nonce_map.insert(5u64, "call_1".to_string());
        let call_ids = vec![("call_1".to_string(), "inspect_path".to_string())];

        let results = map_results_to_tool_responses(stdout, stderr, &nonce_map, &call_ids);
        assert_eq!(results.len(), 1);
        assert!(results[0].2.contains("5c0"));
        assert!(results[0].2.contains("\"content\":\"hello\""));
    }

    #[test]
    fn map_results_with_stderr() {
        let stdout =
            "{\"type\":\"status\",\"nonce\":1,\"status\":\"c\",\"pid\":0,\"exit_code\":1}\n";
        let stderr = "command not found";
        let mut nonce_map = std::collections::HashMap::new();
        nonce_map.insert(1u64, "call_1".to_string());
        let call_ids = vec![("call_1".to_string(), "exec_command".to_string())];

        let results = map_results_to_tool_responses(stdout, stderr, &nonce_map, &call_ids);
        assert_eq!(results.len(), 1);
        assert!(results[0].2.contains("1c1"));
        assert!(results[0].2.contains("stderr: command not found"));
    }

    #[test]
    fn map_results_signal_done() {
        let stdout = "";
        let stderr = "";
        let nonce_map = std::collections::HashMap::new();
        let call_ids = vec![("call_1".to_string(), "signal_done".to_string())];

        let results = map_results_to_tool_responses(stdout, stderr, &nonce_map, &call_ids);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].2, "OK");
    }

    #[test]
    fn map_results_manage_context() {
        let stdout = "";
        let stderr = "";
        let nonce_map = std::collections::HashMap::new();
        let call_ids = vec![("call_1".to_string(), "manage_context".to_string())];

        let results = map_results_to_tool_responses(stdout, stderr, &nonce_map, &call_ids);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].2, "OK");
    }

    #[test]
    fn map_results_multiple_tools() {
        let stdout = "{\"type\":\"status\",\"nonce\":10,\"status\":\"r\",\"pid\":0,\"exit_code\":0}\n{\"type\":\"status\",\"nonce\":10,\"status\":\"c\",\"pid\":0,\"exit_code\":0}\n{\"type\":\"status\",\"nonce\":11,\"status\":\"c\",\"pid\":0,\"exit_code\":0}\n{\"type\":\"result\",\"nonce\":11,\"data\":\"{\\\"exists\\\":true}\"}\n";
        let stderr = "";
        let mut nonce_map = std::collections::HashMap::new();
        nonce_map.insert(10u64, "call_1".to_string());
        nonce_map.insert(11u64, "call_2".to_string());
        let call_ids = vec![
            ("call_1".to_string(), "exec_command".to_string()),
            ("call_2".to_string(), "inspect_path".to_string()),
        ];

        let results = map_results_to_tool_responses(stdout, stderr, &nonce_map, &call_ids);
        assert_eq!(results.len(), 2);
        // exec_command should have its status
        assert!(results[0].2.contains("10c0"));
        // inspect_path should have result data
        assert!(results[1].2.contains("\"exists\":true"));
    }

    #[test]
    fn map_results_empty_output() {
        let stdout = "";
        let stderr = "";
        let mut nonce_map = std::collections::HashMap::new();
        nonce_map.insert(1u64, "call_1".to_string());
        let call_ids = vec![("call_1".to_string(), "exec_command".to_string())];

        let results = map_results_to_tool_responses(stdout, stderr, &nonce_map, &call_ids);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].2, "OK");
    }

    #[test]
    fn shared_external_control_receiver_consumes_turn_controls_once() {
        let bus = EventBus::new();
        let mut control_rx = bus.subscribe();

        bus.send(AppEvent::SteerRequested {
            session_id: Some("codex-thread".to_string()),
            text: "steer once".to_string(),
            id: "s1".to_string(),
        });

        match control_rx.try_recv() {
            Ok(AppEvent::SteerRequested { id, .. }) => assert_eq!(id, "s1"),
            other => panic!("expected control receiver to see steer, got {:?}", other),
        }

        assert!(matches!(
            control_rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        bus.send(AppEvent::SteerRequested {
            session_id: Some("codex-thread".to_string()),
            text: "steer later".to_string(),
            id: "s2".to_string(),
        });

        match control_rx.try_recv() {
            Ok(AppEvent::SteerRequested { id, .. }) => assert_eq!(id, "s2"),
            other => panic!(
                "expected shared receiver to see later steer, got {:?}",
                other
            ),
        }
    }

}

/// Set up a fresh conversation with project context, memory, and skills (without a task).
/// Used by both `setup_fresh_conversation` and the persistent presence conversation.
fn setup_fresh_conversation_no_task(conv: &mut Conversation, project: &Project) {
    // Inject project root so the model knows which directory to work in
    conv.add_user(format!(
        "Working directory: {}\nThis is the project you should examine and modify. \
All relative paths and commands execute from this directory.",
        project.root.display()
    ));
    conv.add_assistant(
        "Understood. I will work within the specified project directory.".to_string(),
    );

    // Inject INTENDANT.md instructions
    if let Some(instructions) = prompts::load_project_instructions(Some(&project.root)) {
        conv.add_user(instructions);
        conv.add_assistant("Acknowledged. I will follow the project instructions.".to_string());
    }

    // Inject knowledge
    if project.config.memory.enabled {
        if let Ok(kstore) = knowledge::load(&project.memory_path()) {
            let refs: Vec<&_> = kstore.entries.iter().collect();
            let msg = knowledge::format_for_injection(&refs);
            if !msg.is_empty() {
                conv.add_user(msg);
                conv.add_assistant(
                    "Acknowledged. I have loaded the project knowledge.".to_string(),
                );
            }
        }
    }

    // Inject skill catalog
    let discovered_skills = skills::discover_skills(Some(&project.root));
    if !discovered_skills.is_empty() {
        let catalog = skills::format_skill_catalog(&discovered_skills);
        conv.add_user(catalog);
        conv.add_assistant("Acknowledged. I see the available skills.".to_string());
    }
}

/// Set up a fresh conversation with project context, memory, skills, and task.
#[allow(dead_code)]
fn setup_fresh_conversation(conv: &mut Conversation, project: &Project, task: &str) {
    setup_fresh_conversation_no_task(conv, project);
    conv.add_user(task.to_string());
}

/// Set up a fresh conversation with project context, memory, skills, task, and
/// optional user-attached images.  When images are present, they are added to
/// the same user message as the task so the model sees them as inline context.
fn setup_fresh_conversation_with_attachments(
    conv: &mut Conversation,
    project: &Project,
    task: &str,
    images: Vec<conversation::ImageData>,
) {
    setup_fresh_conversation_no_task(conv, project);
    if images.is_empty() {
        conv.add_user(task.to_string());
    } else {
        conv.add_user_with_images(task.to_string(), images);
    }
}

/// Resolve `frames:` context hints into HQ images from the frame registry.
async fn resolve_frame_hints(
    hints: &[String],
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
) -> Vec<conversation::ImageData> {
    let mut images = Vec::new();
    for hint in hints {
        if let Some(frame_list) = hint.strip_prefix("frames:") {
            let reg = registry.read().await;
            for fid in frame_list
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
            {
                match reg.read_hq(fid) {
                    Ok(data) => {
                        use base64::Engine;
                        images.push(conversation::ImageData {
                            media_type: "image/jpeg".to_string(),
                            data: base64::engine::general_purpose::STANDARD.encode(&data),
                        });
                    }
                    Err(_) => {
                        // Frame not found — skip silently
                    }
                }
            }
        }
    }
    images
}

/// Resolve explicit frame IDs into HQ images from the frame registry.
async fn resolve_frame_ids(
    frame_ids: &[String],
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
) -> Vec<conversation::ImageData> {
    if frame_ids.is_empty() {
        return Vec::new();
    }
    let mut images = Vec::new();
    let reg = registry.read().await;
    for fid in frame_ids {
        match reg.read_hq(fid) {
            Ok(data) => {
                use base64::Engine;
                images.push(conversation::ImageData {
                    media_type: "image/jpeg".to_string(),
                    data: base64::engine::general_purpose::STANDARD.encode(&data),
                });
            }
            Err(_) => {
                // Frame not found — skip silently
            }
        }
    }
    images
}

/// Resolve frame IDs into `AgentImageAttachment`s for an external agent.
///
/// Captures the on-disk path so backends like Codex can pass `LocalImage`
/// (file reference) instead of inline base64 in JSON-RPC.
#[allow(dead_code)]
async fn resolve_frame_attachments(
    frame_ids: &[String],
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
) -> Vec<external_agent::AgentImageAttachment> {
    if frame_ids.is_empty() {
        return Vec::new();
    }
    let mut atts = Vec::new();
    let reg = registry.read().await;
    for fid in frame_ids {
        let Ok(data) = reg.read_hq(fid) else { continue };
        use base64::Engine;
        let base64 = base64::engine::general_purpose::STANDARD.encode(&data);
        let path = reg.path_for(fid);
        atts.push(external_agent::AgentImageAttachment::from_frame_path(
            path,
            base64,
            "image/jpeg".to_string(),
        ));
    }
    atts
}

/// Resolve a mixed list of attachment identifiers (frames from the live
/// frame registry, uploads from the on-disk store) into the unified
/// `AgentAttachment` shape the backends consume.
///
/// Identifier convention:
/// - `"frame:<id>"` or plain `<id>` — a frame registry entry. Plain ids
///   remain supported for backward compatibility with the existing
///   dashboard path that submits frame ids directly.
/// - `"upload:<id>"` — an upload store descriptor. Images load base64
///   inline (for Gemini ACP); files pass through as `AgentAttachment::File`
///   and the backend's default handling prepends a prelude pointing at the
///   on-disk path.
///
/// Order is preserved from the input list so the prelude reads the files
/// in the order the user selected them.
async fn resolve_attachments(
    ids: &[String],
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    session_dir: &std::path::Path,
    project_root: &std::path::Path,
) -> Vec<external_agent::AgentAttachment> {
    resolve_attachments_with_project_roots(
        ids,
        registry,
        session_dir,
        &[project_root.to_path_buf()],
    )
    .await
}

async fn resolve_attachments_with_project_roots(
    ids: &[String],
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    session_dir: &std::path::Path,
    project_roots: &[PathBuf],
) -> Vec<external_agent::AgentAttachment> {
    if ids.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<external_agent::AgentAttachment> = Vec::with_capacity(ids.len());
    for raw in ids {
        if let Some(upload_id) = raw.strip_prefix("upload:") {
            let Some(d) = project_roots
                .iter()
                .find_map(|root| upload_store::find_upload(upload_id, session_dir, root))
            else {
                continue;
            };
            if d.is_image() {
                // Load the bytes eagerly so Gemini ACP can base64-encode
                // inline. Codex prefers the path.
                let (base64, mime) = match std::fs::read(&d.path) {
                    Ok(bytes) => {
                        use base64::Engine;
                        (
                            base64::engine::general_purpose::STANDARD.encode(&bytes),
                            d.mime.clone(),
                        )
                    }
                    Err(_) => continue,
                };
                out.push(external_agent::AgentAttachment::Image(
                    external_agent::AgentImageAttachment::from_frame_path(
                        d.path.clone(),
                        base64,
                        mime,
                    ),
                ));
            } else {
                out.push(external_agent::AgentAttachment::File(
                    external_agent::AgentFileAttachment {
                        local_path: d.path.clone(),
                        name: d.original_name.clone().unwrap_or_else(|| d.name.clone()),
                        mime_type: d.mime.clone(),
                        size: d.size,
                    },
                ));
            }
            continue;
        }
        // Frame resolution: accept both "frame:<id>" and bare ids for
        // backward compatibility with dashboards that predate the upload
        // feature.
        let fid = raw.strip_prefix("frame:").unwrap_or(raw);
        let (data, path) = {
            let reg = registry.read().await;
            let Ok(data) = reg.read_hq(fid) else {
                continue;
            };
            (data, reg.path_for(fid))
        };
        use base64::Engine;
        let base64 = base64::engine::general_purpose::STANDARD.encode(&data);
        out.push(external_agent::AgentAttachment::Image(
            external_agent::AgentImageAttachment::from_frame_path(
                path,
                base64,
                "image/jpeg".to_string(),
            ),
        ));
    }
    out
}

/// Auto-attach the latest display frame(s) from the frame registry.
async fn auto_attach_display_frames(
    registry: &Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
) -> Vec<conversation::ImageData> {
    let reg = registry.read().await;
    let mut images = Vec::new();
    for stream in reg.active_streams() {
        if stream.starts_with("display_") {
            if let Some(frame_id) = reg.latest(Some(&stream)) {
                if let Ok(data) = reg.read_hq(frame_id) {
                    use base64::Engine;
                    images.push(conversation::ImageData {
                        media_type: "image/jpeg".to_string(),
                        data: base64::engine::general_purpose::STANDARD.encode(&data),
                    });
                }
            }
        }
    }
    images
}

/// Take a fresh screenshot of the user's display for CU-first routing.
/// Tries DisplaySession first (works on Wayland), falls back to platform tools.
async fn capture_display_screenshot(
    log_dir: &std::path::Path,
    session_registry: &display::SharedSessionRegistry,
) -> Option<conversation::ImageData> {
    // Try DisplaySession first — works on Wayland and any display with a session
    if let Some(session) = session_registry.read().await.get(0) {
        if let Ok(png_bytes) = session.screenshot().await {
            let screenshot_path = log_dir.join("cu_reference.png");
            std::fs::write(&screenshot_path, &png_bytes).ok()?;
            use base64::Engine;
            return Some(conversation::ImageData {
                media_type: "image/png".to_string(),
                data: base64::engine::general_purpose::STANDARD.encode(&png_bytes),
            });
        }
    }

    // Fallback: platform-native screenshot tools
    #[cfg(target_os = "linux")]
    crate::linux_display_env::ensure_gui_session_env("fresh display screenshot");

    let screenshot_path = log_dir.join("cu_reference.png");
    let ok = if cfg!(target_os = "macos") {
        tokio::process::Command::new("screencapture")
            .args(["-x", &screenshot_path.to_string_lossy()])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    } else {
        let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
        tokio::process::Command::new("import")
            .args([
                "-window",
                "root",
                "-display",
                &display,
                &screenshot_path.to_string_lossy(),
            ])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if !ok {
        return None;
    }
    let data = std::fs::read(&screenshot_path).ok()?;
    use base64::Engine;
    Some(conversation::ImageData {
        media_type: "image/png".to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(&data),
    })
}

// Try the CU-first path: send task to the fast CU model.
/// Returns None if CU is not available (no display, no provider).
#[allow(clippy::too_many_arguments)]
async fn try_cu_first(
    project_root: &std::path::Path,
    reference_images: &[conversation::ImageData],
    frame_images: &[conversation::ImageData],
    task: &str,
    session_log: &SharedSessionLog,
    log_dir: &std::path::Path,
    bus: &event::EventBus,
    session_registry: &display::SharedSessionRegistry,
) -> Option<Result<CuTaskResult, CallerError>> {
    slog(session_log, |l| {
        l.info(&format!(
            "try_cu_first: ref_images={}, frame_images={}, task={}",
            reference_images.len(),
            frame_images.len(),
            types::truncate_str(task, 60)
        ))
    });

    let reference_images = if reference_images.is_empty() {
        // No frames from browser streaming — try a fresh screenshot if user display
        // is granted, so CU-first can work without the Stream button being active.
        if std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_ok() {
            slog(session_log, |l| {
                l.info("try_cu_first: no registry frames, taking fresh screenshot")
            });
            match capture_display_screenshot(log_dir, session_registry).await {
                Some(img) => vec![img],
                None => {
                    slog(session_log, |l| {
                        l.info("try_cu_first: fresh screenshot failed, returning None")
                    });
                    return None;
                }
            }
        } else {
            slog(session_log, |l| {
                l.info("try_cu_first: no display images and no display grant, returning None")
            });
            return None;
        }
    } else {
        reference_images.to_vec()
    };

    let proj = Project::from_root(project_root.to_path_buf()).ok()?;
    let mut cu_provider = match provider::select_cu_provider(&proj.config.computer_use) {
        Ok(p) => {
            if !p.cu_enabled() {
                slog(session_log, |l| {
                    l.warn("CU provider selected but cu_enabled=false, skipping CU-first")
                });
                return None;
            }
            p
        }
        Err(_) => return None,
    };

    // Override cu_display with the actual display dimensions. The default
    // from select_cu_provider is sized for virtual displays (e.g. 768x1024).
    // On macOS or when targeting the user's real display, the actual resolution
    // may differ (e.g. 1512x949), causing coordinate mismatches.
    if std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_ok() {
        let display_id = std::env::var("DISPLAY")
            .ok()
            .and_then(|d| d.trim_start_matches(':').parse::<u32>().ok())
            .unwrap_or(0);
        let (w, h) = query_display_resolution(display_id);
        if w > 0 && h > 0 {
            slog(session_log, |l| {
                l.info(&format!(
                    "CU display override: {}x{} (actual user display)",
                    w, h
                ))
            });
            cu_provider.set_cu_display((w, h));
        }
    }

    slog(session_log, |l| {
        l.info(&format!(
            "CU-first: {} (provider: {}, model: {})",
            types::truncate_str(task, 80),
            cu_provider.name(),
            cu_provider.model()
        ))
    });
    bus.send(event::AppEvent::PresenceLog {
        message: format!("Trying CU: {}", types::truncate_str(task, 80)),
        level: None,
        turn: None,
    });

    Some(
        run_cu_task(
            cu_provider.as_ref(),
            task,
            reference_images.to_vec(),
            frame_images.to_vec(),
            session_log,
            log_dir,
            bus,
            &proj.config.computer_use,
            None, // auto-resolve display target
            Some(session_registry),
        )
        .await,
    )
}

/// Spawn a listener that reacts to display grant/revoke events.
/// On grant: create a DisplaySession (Wayland) and emit DisplayReady.
/// On revoke: stop the session and remove it from the registry.
pub fn spawn_user_display_listener(
    bus: EventBus,
    session_registry: display::SharedSessionRegistry,
    frame_registry: Option<std::sync::Arc<tokio::sync::RwLock<frames::FrameRegistry>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = bus.subscribe();
        loop {
            match rx.recv().await {
                Ok(AppEvent::UserDisplayGranted { display_id }) => {
                    activate_user_display(
                        &bus,
                        &session_registry,
                        frame_registry.clone(),
                        display_id,
                    )
                    .await;
                }
                Ok(AppEvent::UserDisplayRevoked { display_id, .. }) => {
                    deactivate_user_display(&session_registry, display_id).await;
                }
                Ok(AppEvent::DisplayCaptureLost {
                    display_id,
                    ref reason,
                }) => {
                    // Capture backend stopped unexpectedly (portal session
                    // ended, backend crashed, etc.).  Remove the session from
                    // the registry so a re-grant creates a fresh one.
                    eprintln!(
                        "[user_display] Capture lost for display {}: {}",
                        display_id, reason,
                    );
                    if let Some(session) = session_registry.write().await.remove(display_id) {
                        session.stop().await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                _ => {}
            }
        }
    })
}

/// Tear down a user display session on revoke.
///
/// Registry removal is the only part that has to complete before the
/// caller returns — once the session is out of the registry, no new
/// offer can find it. `session.stop()` then tears down the capture,
/// encoder, and clipboard tasks, which can take many seconds (each
/// awaits a thread join). We run that in the background so the
/// caller — `spawn_user_display_listener`'s `rx.recv()` loop — can
/// pick up the next event (e.g. a follow-up `UserDisplayGranted`
/// from a user who toggled off and back on) without waiting for the
/// old session's threads to exit. Before this, a toggle-off-then-on
/// cycle serialized behind `session.stop().await` — "turn on, wait
/// 20+s, turn on is instant" mapped exactly to "the old stop finally
/// finished and the listener got to the new grant".
async fn deactivate_user_display(
    session_registry: &display::SharedSessionRegistry,
    display_id: u32,
) {
    if let Some(session) = session_registry.write().await.remove(display_id) {
        eprintln!(
            "[user_display] Stopping display session for :{}",
            display_id
        );
        tokio::spawn(async move {
            session.stop().await;
        });
    }
}

fn report_user_display_capture_unavailable(
    bus: &EventBus,
    display_id: u32,
    reason: impl Into<String>,
) {
    let reason = reason.into();
    eprintln!("[user_display] {reason}");
    bus.send(AppEvent::DisplayCaptureLost { display_id, reason });
}

/// Handle user display grant: create a `DisplaySession` and emit
/// `DisplayReady` for the selected user display.
///
/// `target_display_id` is the intendant-stable display ID (0 = primary).
/// This wires the user's display into the same lifecycle as virtual displays —
/// the recording listener starts ffmpeg and the web dashboard shows a display slot.
async fn activate_user_display(
    bus: &EventBus,
    session_registry: &display::SharedSessionRegistry,
    frame_registry: Option<std::sync::Arc<tokio::sync::RwLock<frames::FrameRegistry>>>,
    target_display_id: u32,
) {
    let display_id: u32 = target_display_id;

    if let Some(session) = session_registry.read().await.get(display_id) {
        let (width, height) = session.resolution();
        eprintln!(
            "[user_display] Display :{} capture already active ({}x{}); skipping activation",
            display_id, width, height
        );
        bus.send(AppEvent::DisplayReady {
            display_id,
            width,
            height,
        });
        return;
    }

    #[cfg(target_os = "linux")]
    crate::linux_display_env::ensure_gui_session_env("user display activation");

    // On Wayland: create a DisplaySession with WaylandBackend.
    // Detect Wayland even when WAYLAND_DISPLAY isn't in our env (e.g. started
    // from a tty/ssh session while a graphical session is active).
    #[cfg(target_os = "linux")]
    let wayland_session_detected =
        std::env::var("WAYLAND_DISPLAY").is_ok() || detect_wayland_socket().is_some();

    #[cfg(target_os = "linux")]
    if wayland_session_detected {
        if let Some(socket) = detect_wayland_socket() {
            if std::env::var("WAYLAND_DISPLAY").is_err() {
                eprintln!(
                    "[user_display] WAYLAND_DISPLAY not set, detected socket: {}",
                    socket
                );
                std::env::set_var("WAYLAND_DISPLAY", &socket);
            }
            if std::env::var("XDG_RUNTIME_DIR").is_err() {
                let uid = crate::platform::current_uid();
                let runtime_dir = format!("/run/user/{}", uid);
                std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir);
            }
        }
        eprintln!("[user_display] Requesting Wayland screen capture via XDG portal...");
        eprintln!(
            "[user_display] A screen-sharing dialog should appear on the display — \
             enable Allow Remote Interaction, then approve it to enable video capture \
             and Computer Use input"
        );
        let backend = display::wayland::WaylandBackend::new();
        let session = display::DisplaySession::new(display_id, Arc::new(backend));
        // The portal dialog requires user interaction on the physical display.
        // If the user is accessing intendant remotely (web dashboard, SSH) they
        // may never see the dialog, so emit a status event for the dashboard to
        // surface a banner — and apply a generous timeout to avoid hanging
        // forever, falling through to X11 capture if the user never approves.
        bus.send(AppEvent::DisplayApprovalPending {
            display_id,
            backend: "wayland",
        });
        const WAYLAND_PORTAL_APPROVAL_TIMEOUT_SECS: u64 = 300;
        match tokio::time::timeout(
            std::time::Duration::from_secs(WAYLAND_PORTAL_APPROVAL_TIMEOUT_SECS),
            session.start(30, frame_registry.clone(), Some(bus.clone())),
        )
        .await
        {
            Ok(Ok(())) => {
                // Use the backend's resolution (from portal), not xdpyinfo.
                let (width, height) = session.resolution();
                let session = Arc::new(session);
                session.spawn_metrics_logger(Some(bus.clone()));
                session_registry.write().await.insert(display_id, session);
                bus.send(AppEvent::DisplayReady {
                    display_id,
                    width,
                    height,
                });
                return;
            }
            Ok(Err(e)) => {
                report_user_display_capture_unavailable(
                    bus,
                    display_id,
                    format!(
                        "Wayland portal activation failed: {e}. Re-request user display access \
                         and approve the GNOME portal with Allow Remote Interaction enabled."
                    ),
                );
                return;
            }
            Err(_) => {
                report_user_display_capture_unavailable(
                    bus,
                    display_id,
                    format!(
                        "Wayland portal timed out after {WAYLAND_PORTAL_APPROVAL_TIMEOUT_SECS}s \
                         (screen-sharing dialog was not approved). Re-request user display access \
                         and approve the GNOME portal with Allow Remote Interaction enabled."
                    ),
                );
                return;
            }
        }
    }

    // X11: detect display and create a DisplaySession with X11Backend.
    #[cfg(target_os = "linux")]
    {
        let has_x11 = std::env::var("DISPLAY").is_ok() || vision::detect_x11_display().is_some();
        if has_x11 {
            // Ensure DISPLAY is set for downstream X11 capture/input paths.
            if std::env::var("DISPLAY").is_err() {
                if let Some(d) = vision::detect_x11_display() {
                    std::env::set_var("DISPLAY", &d);
                }
            }
            // If a specific display was requested, look it up from xrandr
            // enumeration and use X11Backend::with_display() for the
            // matching X display string (e.g. ":0", ":1").
            let backend = if target_display_id != 0 {
                let displays = display::enumerate_displays().await;
                if let Some(info) = displays.iter().find(|d| d.id == target_display_id) {
                    eprintln!(
                        "[user_display] X11: requested display_id={}, matched '{}'",
                        target_display_id, info.name,
                    );
                    // X11 monitors share the same DISPLAY string -- the
                    // root window spans all monitors.  The enumerated
                    // displays from xrandr are sub-regions of the same
                    // root.  We still create a standard backend capturing
                    // the root window; the per-monitor distinction is used
                    // for coordinate mapping in the CU layer.
                    display::x11::X11Backend::new()
                        .map_err(|e| eprintln!("[user_display] X11 backend failed: {}", e))
                } else {
                    eprintln!(
                        "[user_display] X11: display_id={} not found, falling back to default",
                        target_display_id,
                    );
                    display::x11::X11Backend::new()
                        .map_err(|e| eprintln!("[user_display] X11 backend failed: {}", e))
                }
            } else {
                display::x11::X11Backend::new()
                    .map_err(|e| eprintln!("[user_display] X11 backend failed: {}", e))
            };
            if let Ok(backend) = backend {
                let session = display::DisplaySession::new(display_id, Arc::new(backend));
                if let Err(e) = session
                    .start(30, frame_registry.clone(), Some(bus.clone()))
                    .await
                {
                    eprintln!("[user_display] X11 display session failed: {}", e);
                } else {
                    if wayland_session_detected && x11_fallback_session_is_all_black(&session).await
                    {
                        session.stop().await;
                        report_user_display_capture_unavailable(
                            bus,
                            display_id,
                            "Wayland portal was not approved and X11 fallback captured an \
                             all-black rootless Xwayland frame. Approve the screen-sharing \
                             portal for the user session, or target a virtual Xvfb display \
                             for headed harness work."
                                .to_string(),
                        );
                        return;
                    }
                    let (width, height) = session.resolution();
                    let session = Arc::new(session);
                    session.spawn_metrics_logger(Some(bus.clone()));
                    session_registry.write().await.insert(display_id, session);
                    bus.send(AppEvent::DisplayReady {
                        display_id,
                        width,
                        height,
                    });
                    return;
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        // If a specific display was requested, resolve its platform_id (CGDisplayID)
        // from the enumerated list; macOS window entries are synthetic display
        // IDs whose platform_id is the CGWindowID.
        let backend = if target_display_id != 0 {
            let displays = display::enumerate_displays().await;
            if let Some(info) = displays.iter().find(|d| d.id == target_display_id) {
                match info.kind {
                    display::DisplayInfoKind::Display => {
                        display::macos::MacOSBackend::with_display_id(info.platform_id as u32)
                    }
                    display::DisplayInfoKind::Window => {
                        display::macos::MacOSBackend::with_window_id(info.platform_id as u32)
                    }
                }
            } else if let Some(window_id) =
                display::macos::window_id_from_display_id(target_display_id)
            {
                display::macos::MacOSBackend::with_window_id(window_id)
            } else {
                report_user_display_capture_unavailable(
                    bus,
                    display_id,
                    format!("display {target_display_id} is not available on this Mac"),
                );
                return;
            }
        } else {
            display::macos::MacOSBackend::new()
        };
        let session = display::DisplaySession::new(display_id, Arc::new(backend));
        if let Err(e) = session.start(30, frame_registry, Some(bus.clone())).await {
            report_user_display_capture_unavailable(
                bus,
                display_id,
                format!("macOS display session failed: {e}"),
            );
            return;
        } else {
            let (width, height) = session.resolution();
            let session = Arc::new(session);
            session.spawn_metrics_logger(Some(bus.clone()));
            session_registry.write().await.insert(display_id, session);
            bus.send(AppEvent::DisplayReady {
                display_id,
                width,
                height,
            });
            return;
        }
    }

    #[cfg(target_os = "windows")]
    {
        // If a specific display was requested, resolve its platform_id (DXGI
        // output ordinal) from the enumerated list; otherwise capture the
        // primary output. Mirrors the macOS arm.
        let backend = if target_display_id != 0 {
            let displays = display::enumerate_displays().await;
            if let Some(info) = displays.iter().find(|d| d.id == target_display_id) {
                display::windows::WindowsBackend::with_output_index(info.platform_id as u32)
            } else {
                report_user_display_capture_unavailable(
                    bus,
                    display_id,
                    format!("display {target_display_id} is not available on this Windows host"),
                );
                return;
            }
        } else {
            display::windows::WindowsBackend::new()
        };
        let session = display::DisplaySession::new(display_id, Arc::new(backend));
        if let Err(e) = session.start(30, frame_registry, Some(bus.clone())).await {
            report_user_display_capture_unavailable(
                bus,
                display_id,
                format!("Windows display session failed: {e}"),
            );
            return;
        } else {
            let (width, height) = session.resolution();
            let session = Arc::new(session);
            session.spawn_metrics_logger(Some(bus.clone()));
            session_registry.write().await.insert(display_id, session);
            bus.send(AppEvent::DisplayReady {
                display_id,
                width,
                height,
            });
            return;
        }
    }

    #[allow(unreachable_code)]
    {
        report_user_display_capture_unavailable(
            bus,
            display_id,
            "no supported display backend detected",
        );
    }
}

#[cfg(target_os = "linux")]
async fn x11_fallback_session_is_all_black(session: &display::DisplaySession) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if let Some(frame) = session.latest_frame().await {
            return !frame_has_visible_rgb(&frame);
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(target_os = "linux")]
fn frame_has_visible_rgb(frame: &display::Frame) -> bool {
    if frame.width == 0 || frame.height == 0 || frame.stride == 0 {
        return false;
    }
    let row_bytes = frame.width as usize * 4;
    let stride = frame.stride as usize;
    if stride < row_bytes || frame.data.len() < stride.saturating_mul(frame.height as usize) {
        return false;
    }

    let total_pixels = frame.width as usize * frame.height as usize;
    let step = (total_pixels / 4096).max(1);
    let mut pixel_index = 0usize;
    for y in 0..frame.height as usize {
        let row = y * stride;
        for x in 0..frame.width as usize {
            if pixel_index % step == 0 {
                let px = row + x * 4;
                if frame.data[px] > 3 || frame.data[px + 1] > 3 || frame.data[px + 2] > 3 {
                    return true;
                }
            }
            pixel_index += 1;
        }
    }
    false
}

/// Auto-register the Windows desktop as an active display at web-daemon
/// startup, so the dashboard's Video tab streams it on connect — no grant
/// click and no running agent required.
///
/// On macOS and Linux the screen is shared behind a consent gate (TCC, the
/// Wayland portal dialog) or a virtual display is launched on demand, so
/// those platforms keep activating the user display only on an explicit
/// grant. Windows has no such per-session consent step: in the headless /
/// RDP server scenario the existing desktop *is* the always-on stream, and
/// the OS-level capture permission is implicit. We therefore mirror the
/// macOS *end state* (a live `DisplaySession` already in the registry, so a
/// fresh browser connect replays `display_ready` and auto-streams) by
/// activating display 0 up front, reusing the same [`activate_user_display`]
/// machinery — which on Windows captures the existing desktop via
/// `WindowsBackend` (DXGI Desktop Duplication), NOT a virtual Xvfb display.
///
/// The autonomy grant flag and `INTENDANT_USER_DISPLAY_GRANTED` env are set
/// to match a real grant, so the dashboard's "your display" toggle, CU
/// display targeting, and agent subprocesses all observe a consistent
/// "granted" state. Activation degrades gracefully — if the capture backend
/// can't start (no interactive desktop, etc.) `activate_user_display` logs
/// and returns without registering, leaving the dashboard at "No displays
/// active" rather than failing startup.
#[cfg(target_os = "windows")]
async fn auto_activate_windows_user_display(
    bus: &EventBus,
    session_registry: &display::SharedSessionRegistry,
    frame_registry: Option<std::sync::Arc<tokio::sync::RwLock<frames::FrameRegistry>>>,
    autonomy: &SharedAutonomy,
) {
    eprintln!("[user_display] Windows: auto-registering desktop as active display (display 0)");
    {
        let mut guard = autonomy.write().await;
        guard.user_display_granted = true;
    }
    std::env::set_var("INTENDANT_USER_DISPLAY_GRANTED", "1");
    activate_user_display(bus, session_registry, frame_registry, 0).await;
}

/// Detect a Wayland compositor socket even when WAYLAND_DISPLAY is not set.
/// Checks /run/user/<uid>/ for wayland-* sockets.
#[cfg(target_os = "linux")]
fn detect_wayland_socket() -> Option<String> {
    let uid = crate::platform::current_uid();
    let runtime_dir = format!("/run/user/{}", uid);
    let entries = std::fs::read_dir(&runtime_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Match "wayland-0", "wayland-1", etc. but not ".lock" files
        if name.starts_with("wayland-") && !name.ends_with(".lock") {
            if entry.file_type().ok().is_some_and(|ft| {
                use std::os::unix::fs::FileTypeExt;
                ft.is_socket() || ft.is_file()
            }) {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Parse a display target string from the presence model into a `DisplayTarget`.
///
/// Accepts "user_session" for the user's display, or ":<N>" / "<N>" for virtual.
fn parse_display_target_str(s: &str) -> computer_use::DisplayTarget {
    match s.trim() {
        "user_session" | "user" | ":0" | "0" => computer_use::DisplayTarget::UserSession,
        other => {
            let num_str = other.trim_start_matches(':');
            if let Ok(id) = num_str.parse::<u32>() {
                if id == 0 {
                    computer_use::DisplayTarget::UserSession
                } else {
                    computer_use::DisplayTarget::Virtual { id }
                }
            } else {
                // Unrecognized — fall back to auto-resolve
                resolve_cu_display_target()
            }
        }
    }
}

/// Resolve the display target for CU actions.
///
/// If user display access is granted (env var set) and the current DISPLAY
/// is `:0` (or unset, indicating no virtual display was launched), returns
/// `UserSession`. Otherwise returns `Virtual` with the current display ID.
/// On macOS, always returns `UserSession` when DISPLAY is unset (no Xvfb).
fn resolve_cu_display_target() -> computer_use::DisplayTarget {
    let display_id: Option<u32> = std::env::var("DISPLAY")
        .ok()
        .and_then(|d| d.trim_start_matches(':').parse().ok());

    let user_granted = std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_ok();

    match display_id {
        // DISPLAY is :0 and user granted → target user session
        Some(0) if user_granted => computer_use::DisplayTarget::UserSession,
        // DISPLAY is set to a virtual display
        Some(id) => computer_use::DisplayTarget::Virtual { id },
        // No DISPLAY set — if user granted, target their session; else default virtual
        None if user_granted => computer_use::DisplayTarget::UserSession,
        // macOS has no Xvfb — native display is always the target
        None if cfg!(target_os = "macos") => computer_use::DisplayTarget::UserSession,
        None => computer_use::DisplayTarget::Virtual { id: 99 },
    }
}

/// Maximum turns for an ephemeral CU task before giving up.
const CU_TASK_MAX_TURNS: usize = 20;

/// Result of an ephemeral CU task.
enum CuTaskResult {
    /// Task completed by the CU agent.
    Completed(LoopStats),
    /// CU agent determined this isn't a display task; escalate to the full agent.
    Escalate { task: String },
}

/// Run an ephemeral computer-use task with minimal context.
///
/// Creates a lightweight conversation (no project context, skills, or knowledge),
/// runs the CU model for a few turns until the task is done, and returns.
#[allow(clippy::too_many_arguments)]
async fn run_cu_task(
    provider: &dyn provider::ChatProvider,
    task: &str,
    reference_images: Vec<conversation::ImageData>,
    context_images: Vec<conversation::ImageData>,
    session_log: &SharedSessionLog,
    log_dir: &std::path::Path,
    bus: &event::EventBus,
    cu_config: &project::ComputerUseConfig,
    target_override: Option<computer_use::DisplayTarget>,
    session_registry: Option<&display::SharedSessionRegistry>,
) -> Result<CuTaskResult, CallerError> {
    // Owned form for execute_actions, which wants `&Option<_>`.
    let session_registry = session_registry.cloned();
    let mut stats = LoopStats::default();
    let mut cu_counter = 0u64;
    let backend = computer_use::DisplayBackend::from_config(&cu_config.backend);

    let display_target = target_override.unwrap_or_else(resolve_cu_display_target);

    // CU-first system prompt: handle display tasks or escalate
    let system_prompt =
        "You are a fast computer-use agent. You can see and interact with a desktop display.\n\n\
        ROUTING:\n\
        - If the task involves the display (clicking, typing, scrolling, pressing buttons, \
          opening apps, interacting with GUI elements), handle it with your computer use tools.\n\
        - If the task is NOT about the display (coding, file editing, research, shell commands, \
          git, debugging, questions), call escalate_to_agent with the task description.\n\
        - If no display screenshot is provided below, call escalate_to_agent immediately.\n\n\
        WHEN HANDLING DISPLAY TASKS:\n\
        1. Examine the screenshot to identify target elements\n\
        2. Perform the required actions\n\
        3. Take a verification screenshot\n\
        4. Respond with DONE and a one-sentence summary\n\n\
        RULES:\n\
        - Perform ONLY the requested task, nothing else.\n\
        - Once done, STOP. Do not take additional actions.\n\
        - Be precise with coordinates. Act efficiently."
            .to_string();

    // No display frames at all → escalate immediately without API call
    if reference_images.is_empty() && context_images.is_empty() {
        slog(session_log, |l| {
            l.info("CU: no display frames available, escalating")
        });
        return Ok(CuTaskResult::Escalate {
            task: task.to_string(),
        });
    }

    let ref_image_count = reference_images.len();
    let mut conv = Conversation::new(system_prompt, provider.context_window());

    // Inject reference frames
    if !reference_images.is_empty() {
        conv.add_user_with_images(
            "The user was looking at this screen when they made their request:".to_string(),
            reference_images,
        );
        conv.add_assistant(
            "I can see the reference screen. I'll compare this with the current state.".to_string(),
        );
    }

    // Inject context images
    if !context_images.is_empty() {
        conv.add_user_with_images("Additional context:".to_string(), context_images);
        conv.add_assistant("Noted.".to_string());
    }

    // Add the task
    conv.add_user(task.to_string());

    slog(session_log, |l| {
        l.cu_task_start(
            task,
            provider.name(),
            provider.model(),
            provider.cu_enabled(),
            provider.cu_display(),
            ref_image_count,
        )
    });

    for turn in 1..=CU_TASK_MAX_TURNS {
        stats.turns = turn;

        slog(session_log, |l| {
            l.info(&format!("CU turn {} starting", turn))
        });

        let response = provider
            .chat_stream(conv.messages(), &|event| {
                if let provider::StreamEvent::Delta(ref delta) = event {
                    bus.send(AppEvent::PresenceLog {
                        message: format!("[CU] {}", delta),
                        level: None,
                        turn: Some(turn),
                    });
                }
            })
            .await?;

        conv.set_usage(response.usage.clone());

        // Log structured CU turn
        {
            let mut actions_desc: Vec<String> = response
                .cu_calls
                .iter()
                .flat_map(|cu| cu.actions.iter().map(|a| format!("{:?}", a)))
                .collect();
            for tc in &response.tool_calls {
                actions_desc.push(format!(
                    "{}({})",
                    tc.name,
                    types::truncate_str(&tc.arguments, 100)
                ));
            }
            slog(session_log, |l| {
                l.cu_turn(
                    turn,
                    response.content.len(),
                    response.cu_calls.len(),
                    response.tool_calls.len(),
                    response.usage.prompt_tokens,
                    response.usage.completion_tokens,
                    &actions_desc,
                )
            });
        }
        if !response.content.is_empty() {
            slog(session_log, |l| {
                l.info(&format!(
                    "CU turn {} text: {}",
                    turn,
                    types::truncate_str(&response.content, 500)
                ))
            });
        }
        // Check for escalation before processing anything else
        if let Some(esc_call) = response
            .tool_calls
            .iter()
            .find(|tc| tc.name == "escalate_to_agent")
        {
            let args: serde_json::Value =
                serde_json::from_str(&esc_call.arguments).unwrap_or_default();
            let escalated_task = args["task"].as_str().unwrap_or(task).to_string();
            slog(session_log, |l| {
                l.cu_task_error("escalated", Some(&escalated_task))
            });
            return Ok(CuTaskResult::Escalate {
                task: escalated_task,
            });
        }

        // Handle unrecognized function tool calls: return error results so the
        // model knows these tools are not available in CU mode.
        let non_escalate_tools: Vec<_> = response
            .tool_calls
            .iter()
            .filter(|tc| tc.name != "escalate_to_agent")
            .collect();
        if !non_escalate_tools.is_empty() {
            let refs: Vec<conversation::ToolCallRef> = non_escalate_tools
                .iter()
                .map(|tc| conversation::ToolCallRef {
                    id: tc.id.clone(),
                    call_id: tc.id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                })
                .collect();
            conv.add_assistant_tool_calls(
                response.content.clone(),
                refs,
                response.raw_output.clone(),
            );
            for tc in &non_escalate_tools {
                slog(session_log, |l| {
                    l.warn(&format!(
                        "CU turn {}: unrecognized tool '{}' — returning error result",
                        turn, tc.name
                    ))
                });
                conv.add_tool_result(
                    &tc.id,
                    &tc.name,
                    &format!(
                        "Error: tool '{}' is not available in computer-use mode. \
                         Use your native computer use actions (click, type, scroll, screenshot) \
                         or call escalate_to_agent to hand off to the coding agent.",
                        tc.name
                    ),
                );
            }
            continue; // let model see the error results
        }

        // Check for task completion
        let content_lower = response.content.to_lowercase();
        let is_done = content_lower.contains("done")
            && response.cu_calls.is_empty()
            && response.tool_calls.is_empty();

        // Store assistant message
        if !response.cu_calls.is_empty() {
            // CU calls: store as assistant with tool call refs
            let refs: Vec<conversation::ToolCallRef> = response
                .cu_calls
                .iter()
                .map(|cu| conversation::ToolCallRef {
                    id: cu.call_id.clone(),
                    call_id: cu.call_id.clone(),
                    name: "computer".to_string(),
                    arguments: String::new(),
                })
                .collect();
            conv.add_assistant_tool_calls(
                response.content.clone(),
                refs,
                response.raw_output.clone(),
            );
        } else {
            conv.add_assistant(response.content.clone());
        }

        if is_done {
            let summary = types::truncate_str(&response.content, 200);
            slog(session_log, |l| l.cu_task_complete(turn, true, summary));
            break;
        }

        // Execute CU calls
        if !response.cu_calls.is_empty() {
            for cu_call in &response.cu_calls {
                slog(session_log, |l| {
                    l.info(&format!(
                        "CU turn {}: {} action(s)",
                        turn,
                        cu_call.actions.len()
                    ))
                });

                let results = computer_use::execute_actions(
                    &cu_call.actions,
                    display_target,
                    backend,
                    log_dir,
                    &mut cu_counter,
                    &session_registry,
                    None,
                )
                .await;

                let last_screenshot = results.iter().rev().find_map(|r| r.screenshot.as_ref());
                let output = if results.iter().all(|r| r.success) {
                    "Actions executed successfully.".to_string()
                } else {
                    let errors: Vec<&str> =
                        results.iter().filter_map(|r| r.error.as_deref()).collect();
                    format!("Some actions failed: {}", errors.join("; "))
                };

                if let Some(screenshot) = last_screenshot {
                    let images = vec![conversation::ImageData {
                        media_type: "image/png".to_string(),
                        data: screenshot.base64_png.clone(),
                    }];
                    conv.add_cu_result(&cu_call.call_id, &output, images);
                } else {
                    conv.add_cu_result(&cu_call.call_id, &output, vec![]);
                }
            }
            continue; // next turn — let model see the results
        }

        // No CU calls and not done — model may be thinking or confused
        if response.cu_calls.is_empty() && response.tool_calls.is_empty() && !is_done {
            slog(session_log, |l| {
                l.cu_task_error(
                    &format!("CU turn {}: no actions returned (text-only response)", turn),
                    None,
                )
            });
        }
        if turn >= CU_TASK_MAX_TURNS {
            slog(session_log, |l| {
                l.cu_task_error("CU task hit max turns", None)
            });
        }
    }

    Ok(CuTaskResult::Completed(stats))
}

/// Execute native computer-use tool calls via the platform-native executor
/// and add results (with screenshots) to the conversation.
/// Handle native `shared_view` tool calls: dashboard visibility into
/// agent-owned displays (sandboxes, VMs, virtual displays). Sharing the
/// user's own screen is explicit opt-in — unlike the MCP path, this handler
/// refuses to flip the display grant itself and instead tells the model the
/// user must grant the display first; input authority is only ever granted
/// by the user from the dashboard.
#[allow(clippy::too_many_arguments)]
async fn handle_shared_view_calls(
    shared_view_calls: &[(String, serde_json::Value)],
    conversation: &mut conversation::Conversation,
    bus: &EventBus,
    autonomy: &SharedAutonomy,
    session_registry: Option<&display::SharedSessionRegistry>,
    session_id: Option<String>,
    log_dir: &std::path::Path,
    cu_counter: &mut u64,
    session_log: &SharedSessionLog,
) {
    for (call_id, args) in shared_view_calls {
        let action = args
            .get("action")
            .and_then(|a| a.as_str())
            .unwrap_or_default();
        let display_target = args
            .get("display_target")
            .and_then(|s| s.as_str())
            .map(str::to_string);
        let reason = args
            .get("reason")
            .and_then(|s| s.as_str())
            .map(str::to_string);
        let region = args.get("region").and_then(|r| {
            Some(mcp::normalize_shared_view_region_xywh(
                r.get("x")?.as_f64()?,
                r.get("y")?.as_f64()?,
                r.get("width")?.as_f64()?,
                r.get("height")?.as_f64()?,
            ))
        });

        let resolved_target = mcp::shared_view_display_target(display_target, None);
        let display_id = mcp::shared_view_display_id(resolved_target.as_deref(), None);
        let label = mcp::shared_view_target_label(display_id, resolved_target.as_deref());

        // The user's own screen is an explicit opt-in path: require the
        // existing display grant instead of flipping it from a tool call.
        // Only display-exposing verbs gate — focus/input/hide operate on
        // whatever view is already shown.
        let effective_user_display = match display_id {
            Some(0) => true,
            Some(_) => false,
            None => matches!(
                resolve_cu_display_target(),
                computer_use::DisplayTarget::UserSession
            ),
        };
        if matches!(action, "show" | "capture")
            && effective_user_display
            && !autonomy.read().await.user_display_granted
        {
            conversation.add_tool_result(
                call_id,
                "shared_view",
                "Error: sharing the user's own screen (user_session) is an explicit opt-in — \
                 the user must grant their display first (dashboard grant or \
                 grant_user_display). Share an agent-owned display instead, e.g. \
                 display_target \"99\" for the virtual display you are working on.",
            );
            continue;
        }

        let emit = |action: &str, note: Option<String>| AppEvent::SharedView {
            session_id: session_id.clone(),
            action: action.to_string(),
            display_target: resolved_target.clone(),
            display_id,
            reason: reason.clone(),
            region: region.clone(),
            note,
        };

        let output = match action {
            "show" => {
                // (Re)activate a granted user display whose session is gone;
                // the grant listener owns the platform work.
                if display_id == Some(0) {
                    let session_missing = match session_registry {
                        Some(registry) => registry.read().await.get(0).is_none(),
                        None => false,
                    };
                    if session_missing {
                        bus.send(AppEvent::UserDisplayGranted { display_id: 0 });
                    }
                }
                bus.send(emit("show", None));
                format!("Shared view shown for {label} — the dashboard is now streaming it.")
            }
            "focus" => match region {
                Some(_) => {
                    bus.send(emit("focus", None));
                    format!("Focus highlighted on {label}.")
                }
                None => "Error: focus requires a region {x, y, width, height} with 0.0-1.0 \
                         fractions."
                    .to_string(),
            },
            "capture" => {
                bus.send(emit("capture", None));
                let target = match display_id {
                    Some(0) => computer_use::DisplayTarget::UserSession,
                    Some(id) => computer_use::DisplayTarget::Virtual { id },
                    None => resolve_cu_display_target(),
                };
                let screenshot_dir = log_dir.join("screenshots");
                let _ = std::fs::create_dir_all(&screenshot_dir);
                let registry = session_registry.cloned();
                let results = computer_use::execute_actions(
                    &[computer_use::CuAction::Screenshot],
                    target,
                    computer_use::DisplayBackend::detect(),
                    &screenshot_dir,
                    cu_counter,
                    &registry,
                    None,
                )
                .await;
                match results.first().and_then(|r| r.screenshot.as_ref()) {
                    Some(shot) => {
                        let images = vec![conversation::ImageData {
                            media_type: "image/png".to_string(),
                            data: shot.base64_png.clone(),
                        }];
                        conversation.add_tool_result_with_images(
                            call_id,
                            "shared_view",
                            &format!("Captured the current frame of {label}."),
                            images,
                        );
                        continue;
                    }
                    None => format!(
                        "Error: no frame available for {label}: {}",
                        results
                            .first()
                            .and_then(|r| r.error.as_deref())
                            .unwrap_or("unknown capture failure")
                    ),
                }
            }
            "input" => {
                bus.send(emit("input", None));
                format!(
                    "Input authority requested for {label}. The user must accept from the \
                     dashboard control — continue only after they take over or respond."
                )
            }
            "hide" => {
                bus.send(emit("hide", None));
                "Shared view dismissed.".to_string()
            }
            other => format!(
                "Error: unknown shared_view action '{other}' — use show, focus, capture, \
                 input, or hide."
            ),
        };
        slog(session_log, |l| {
            l.info(&format!("shared_view {action}: {label}"))
        });
        conversation.add_tool_result(call_id, "shared_view", &output);
    }
}

async fn execute_cu_calls(
    cu_calls: &[computer_use::CuToolCall],
    conversation: &mut conversation::Conversation,
    cu_display: Option<(u32, u32)>,
    log_dir: &std::path::Path,
    counter: &mut u64,
    session_log: &SharedSessionLog,
    session_registry: Option<&display::SharedSessionRegistry>,
) {
    // Owned form for execute_actions, which wants `&Option<_>`.
    let session_registry = session_registry.cloned();
    let display_target = if cu_display.is_some() {
        resolve_cu_display_target()
    } else {
        // No CU display configured — default to virtual :99
        computer_use::DisplayTarget::Virtual { id: 99 }
    };

    for cu_call in cu_calls {
        // Build human-readable description of CU actions
        let action_descs: Vec<String> = cu_call
            .actions
            .iter()
            .map(|a| match a {
                computer_use::CuAction::Click { x, y, button } => {
                    format!("click({},{} {:?})", x, y, button)
                }
                computer_use::CuAction::DoubleClick { x, y, .. } => {
                    format!("double_click({},{})", x, y)
                }
                computer_use::CuAction::Type { text } => {
                    format!("type(\"{}\")", types::truncate_str(text, 50))
                }
                computer_use::CuAction::Key { key } => format!("key({})", key),
                computer_use::CuAction::Scroll {
                    x,
                    y,
                    direction,
                    amount,
                } => format!("scroll({},{} {:?} {})", x, y, direction, amount),
                computer_use::CuAction::MoveMouse { x, y } => format!("move({},{})", x, y),
                computer_use::CuAction::Drag {
                    start_x,
                    start_y,
                    end_x,
                    end_y,
                } => format!("drag({},{}->{},{})", start_x, start_y, end_x, end_y),
                computer_use::CuAction::TripleClick { x, y, .. } => {
                    format!("triple_click({},{})", x, y)
                }
                computer_use::CuAction::MouseDown { x, y, .. } => {
                    format!("mouse_down({},{})", x, y)
                }
                computer_use::CuAction::MouseUp { x, y, .. } => format!("mouse_up({},{})", x, y),
                computer_use::CuAction::Paste { text } => {
                    format!("paste(\"{}\")", types::truncate_str(text, 50))
                }
                computer_use::CuAction::HoldKey { key, ms } => {
                    format!("hold_key({},{}ms)", key, ms)
                }
                computer_use::CuAction::Zoom {
                    x,
                    y,
                    width,
                    height,
                } => format!("zoom({},{} {}x{})", x, y, width, height),
                computer_use::CuAction::Screenshot => "screenshot".to_string(),
                computer_use::CuAction::Wait { ms } => format!("wait({}ms)", ms),
            })
            .collect();
        let desc = action_descs.join(" → ");
        slog(session_log, |l| l.info(&format!("CU: {}", desc)));

        let backend = computer_use::DisplayBackend::detect();
        let results = computer_use::execute_actions(
            &cu_call.actions,
            display_target,
            backend,
            log_dir,
            counter,
            &session_registry,
            None,
        )
        .await;

        // Find the last screenshot from results
        let last_screenshot = results.iter().rev().find_map(|r| r.screenshot.as_ref());
        let output = if results.iter().all(|r| r.success) {
            "Actions executed successfully.".to_string()
        } else {
            let errors: Vec<&str> = results.iter().filter_map(|r| r.error.as_deref()).collect();
            format!("Some actions failed: {}", errors.join("; "))
        };

        if let Some(screenshot) = last_screenshot {
            let images = vec![conversation::ImageData {
                media_type: "image/png".to_string(),
                data: screenshot.base64_png.clone(),
            }];
            conversation.add_cu_result(&cu_call.call_id, &output, images);
        } else {
            conversation.add_cu_result(&cu_call.call_id, &output, vec![]);
        }
    }
}

fn is_simple_task(task: &str) -> bool {
    // A simple task is a single line with no complex indicators
    let lines: Vec<&str> = task.lines().collect();
    if lines.len() > 3 {
        return false;
    }

    let lower = task.to_lowercase();
    let complex_indicators = [
        "research",
        "investigate",
        "implement",
        "build",
        "refactor",
        "migrate",
        "deploy",
        "set up",
        "analyze",
        "compare",
        "design",
        "create a",
    ];

    for indicator in &complex_indicators {
        if lower.contains(indicator) {
            return false;
        }
    }

    // Short tasks are simple
    task.len() < 100
}

fn configure_sandbox_env(flags: &CliFlags, project: &Project, log_dir: &std::path::Path) {
    let enabled = flags.sandbox || project.config.sandbox.enabled;
    if !enabled {
        env::remove_var("INTENDANT_SANDBOX_WRITE_PATHS");
        return;
    }

    let mut sandbox_cfg = sandbox::SandboxConfig::default_for_project(&project.root, log_dir);
    for p in &project.config.sandbox.extra_write_paths {
        let extra = if std::path::Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            project.root.join(p)
        };
        sandbox_cfg.write_paths.push(extra);
    }
    sandbox_cfg.write_paths.sort();
    sandbox_cfg.write_paths.dedup();

    // Platform-correct list encoding (':' on Unix, ';' on Windows — Windows
    // paths contain ':') via env::join_paths. A path containing the list
    // separator cannot be encoded; drop it loudly — the runtime then simply
    // never allows writes there (fail-closed).
    let encodable: Vec<&PathBuf> = sandbox_cfg
        .write_paths
        .iter()
        .filter(|p| {
            let ok = env::join_paths([p]).is_ok();
            if !ok {
                eprintln!(
                    "[sandbox] write path {} contains the PATH separator and cannot                      be passed to the runtime; writes there will be denied",
                    p.display()
                );
            }
            ok
        })
        .collect();
    match env::join_paths(encodable) {
        Ok(joined) => env::set_var("INTENDANT_SANDBOX_WRITE_PATHS", joined),
        Err(e) => {
            eprintln!("[sandbox] failed to encode write paths ({e}); sandbox disabled");
            env::remove_var("INTENDANT_SANDBOX_WRITE_PATHS");
        }
    }

    // Windows: the runtime child enforces writes via a WRITE_RESTRICTED
    // token, which needs RESTRICTED-write ACEs on the allowed paths. Stamp
    // them once for the daemon's lifetime (per-spawn stamping would race
    // concurrent runtime spawns sharing these paths); the guard's Drop and
    // the startup journal sweep handle removal.
    #[cfg(windows)]
    {
        static DAEMON_WRITE_GRANTS: std::sync::Mutex<Option<win_sandbox::AceGuard>> =
            std::sync::Mutex::new(None);
        match win_sandbox::stamp_daemon_write_grants(&sandbox_cfg.write_paths) {
            Ok(guard) => {
                *DAEMON_WRITE_GRANTS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(guard);
            }
            Err(e) => {
                // Fail closed: without the grants the restricted runtime
                // cannot write anywhere and its operations will error.
                eprintln!(
                    "[sandbox] failed to stamp Windows write grants ({e});                      sandboxed runtime writes will be denied"
                );
            }
        }
    }
}

/// The `--scoped-shell-exec` wrapper (see terminal::scoped_shell_command):
/// confine this PTY to the filesystem scope in INTENDANT_SCOPED_SHELL_POLICY
/// and run the shell given in argv. Never returns — on any failure it exits
/// non-zero with a message on stderr (which lands in the terminal pane),
/// and it FAILS CLOSED rather than running an unconfined shell.
///
/// Linux: apply Landlock to this process, then exec the shell in place.
/// Windows: stamp temporary RESTRICTED ACEs on the scope roots, spawn the
/// shell under a fully-restricted token (inheriting this wrapper's ConPTY),
/// wait, remove the ACEs, and exit with the shell's code — see
/// win_sandbox.rs for the model. macOS never reaches this wrapper (scoped
/// shells run under sandbox-exec directly).
fn run_scoped_shell_exec() -> ! {
    let fail = |message: String| -> ! {
        eprintln!("scoped shell: {message}");
        std::process::exit(1);
    };
    #[cfg(target_os = "linux")]
    {
        let policy_json = match env::var(terminal::SCOPED_SHELL_POLICY_ENV) {
            Ok(value) => value,
            Err(_) => fail(format!(
                "{} is not set; this mode is spawned internally by the daemon",
                terminal::SCOPED_SHELL_POLICY_ENV
            )),
        };
        let policy: terminal::ScopedShellPolicy = match serde_json::from_str(&policy_json) {
            Ok(policy) => policy,
            Err(e) => fail(format!("invalid sandbox policy: {e}")),
        };
        let config = sandbox::SandboxConfig {
            read_paths: policy.read,
            write_paths: policy.write,
            enabled: true,
        };
        match config.apply_to_current_process() {
            Ok(true) => {}
            Ok(false) => fail(
                "this kernel does not support Landlock, so the filesystem scope on your \
                 grant cannot be enforced; refusing to start an unconfined shell"
                    .to_string(),
            ),
            Err(e) => fail(format!("applying Landlock failed: {e}")),
        }

        let args: Vec<String> = env::args().skip(2).collect();
        let Some((shell, shell_args)) = args.split_first() else {
            fail("no shell given".to_string());
        };
        use std::os::unix::process::CommandExt as _;
        let mut command = std::process::Command::new(shell);
        command
            .args(shell_args)
            .env_remove(terminal::SCOPED_SHELL_POLICY_ENV);
        let e = command.exec();
        fail(format!("exec {shell}: {e}"));
    }
    #[cfg(windows)]
    {
        // The scope-root ACEs were stamped daemon-side (held by the
        // PtySession) — this wrapper only creates the restricted token,
        // runs the shell under the ConPTY it inherited, and proxies the
        // exit code.
        let args: Vec<String> = env::args().skip(2).collect();
        let Some((shell, shell_args)) = args.split_first() else {
            fail("no shell given".to_string());
        };
        match win_sandbox::run_scoped_shell(shell, shell_args) {
            Ok(code) => std::process::exit(code),
            Err(e) => fail(format!("Windows scoped shell failed: {e}")),
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        fail(
            "--scoped-shell-exec is the Linux/Windows scoped-shell wrapper; macOS scoped \
             shells run under sandbox-exec directly"
                .to_string(),
        );
    }
}

#[tokio::main]
async fn main() -> Result<(), CallerError> {
    // Install the process-wide rustls `CryptoProvider`. **Required
    // by rustls 0.23+**: without this, the first DTLS handshake
    // (typically when the WebRTC driver answers a federated peer's
    // offer — see `display::webrtc::driver`) panics with
    //   "Could not automatically determine the process-level
    //    CryptoProvider from Rustls crate features."
    // The panic kills the worker thread, the in-flight encoder is
    // torn down, and every subsequent offer also panics. Tests
    // call this via the `ensure_rustls_crypto_provider` helper in
    // `display::webrtc::tests`; production never installed it,
    // which surfaced during the 4d.3 E2E smoke test.
    //
    // `install_default()` returns `Err(Arc<CryptoProvider>)` if a
    // provider is already installed (idempotent); we ignore that.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Materialized OAuth credentials must never outlive the process:
    // this guard revokes all leases on every normal return from main
    // (the signal handler and the startup sweep cover the other exits).
    let _lease_shutdown_guard = credential_leases::LeaseShutdownGuard::new();

    // Panic hook: handle broken pipe gracefully and persist panic info
    // to the active session's log directory for post-mortem auditing.
    {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Broken pipe from println!/write! — exit cleanly
            let is_broken_pipe = if let Some(s) = info.payload().downcast_ref::<String>() {
                s.contains("Broken pipe")
            } else if let Some(s) = info.payload().downcast_ref::<&str>() {
                s.contains("Broken pipe")
            } else {
                false
            };
            if is_broken_pipe {
                std::process::exit(0);
            }

            // Write panic info to the session log directory if available.
            // This makes panics discoverable by audit agents alongside
            // session.jsonl and transcript files — no need to hunt for
            // app-backend.log or stderr captures.
            if let Some(dir) = PANIC_LOG_DIR.get() {
                let panic_path = dir.join("panic.log");
                let msg = format!(
                    "{}\n\nBacktrace:\n{:?}\n",
                    info,
                    std::backtrace::Backtrace::force_capture(),
                );
                let _ = std::fs::write(&panic_path, &msg);
            }

            default_hook(info);
        }));
    }

    // Ensure platform tool directories (Homebrew etc.) are in PATH.
    platform::ensure_tool_paths();

    // Internal wrapper mode for filesystem-scoped dashboard shells (Linux):
    // `intendant --scoped-shell-exec <shell> [args…]` with the sandbox
    // policy in INTENDANT_SCOPED_SHELL_POLICY. Applies Landlock to this
    // process (fail-closed) and execs the shell. Spawned only by
    // terminal::PtySession — not a user-facing command.
    if env::args().nth(1).as_deref() == Some("--scoped-shell-exec") {
        run_scoped_shell_exec();
    }

    // Windows: replay ACE journals orphaned by crashed scoped-shell
    // wrappers or runtime parents, so temporary RESTRICTED grants never
    // outlive a crash (see win_sandbox.rs).
    #[cfg(windows)]
    win_sandbox::sweep_stale_journals(&win_sandbox::journal_dir());

    // `intendant lan` was removed when the native dashboard certificate flow
    // became `intendant access`. Fail explicitly so the old command cannot be
    // misread as an ordinary task prompt.
    if env::args().nth(1).as_deref() == Some("lan") {
        eprintln!("error: `intendant lan` was removed; use `intendant access`");
        std::process::exit(1);
    }

    // Intercept `intendant org init <handle>` — creates (or prints) an org
    // root key on this daemon. Like `access`, this is a local path with no
    // project or provider setup. See docs/src/trust-architecture.md.
    if env::args().nth(1).as_deref() == Some("org") {
        let action = env::args().nth(2).unwrap_or_default();
        let handle = env::args().nth(3).unwrap_or_default();
        if action != "init" || handle.trim().is_empty() {
            eprintln!("usage: intendant org init <handle>");
            std::process::exit(2);
        }
        let cert_dir = access::backend::select_backend().cert_dir();
        return match access::org::load_or_create_org_identity(&cert_dir, handle.trim()) {
            Ok(identity) => {
                println!("org handle:   {}", handle.trim());
                println!("org root key: {}", identity.public_key_b64u());
                println!(
                    "key file:     {}",
                    access::org::org_key_path(&cert_dir, handle.trim()).display()
                );
                println!();
                println!(
                    "Daemons accept this org's grants after a root session trusts the key\n                     (Access → Advanced → Organizations, or POST /api/access/orgs/trust).\n                     Issue member grants from this daemon's Access page or\n                     POST /api/access/org-grants/issue."
                );
                Ok(())
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        };
    }

    // Intercept `intendant service <action>` — install/remove/inspect the
    // boot service for this binary (native supervisor per platform:
    // systemd / launchd / Task Scheduler / cron @reboot). Local path, no
    // project or provider setup. `service run` is the built-in
    // supervisor loop the Task Scheduler and cron backends point at.
    if env::args().nth(1).as_deref() == Some("service") {
        let args: Vec<String> = env::args().skip(2).collect();
        std::process::exit(service_mode::run_service_cli(&args));
    }

    // Intercept `intendant access <action>` before the main runtime setup.
    // This is a local certificate/enrollment path with no project, no .env,
    // and no provider selection.
    if env::args().nth(1).as_deref() == Some("access") {
        #[cfg(not(target_os = "windows"))]
        {
            let argv: Vec<String> = env::args().skip(2).collect();
            return match access::run(argv).await {
                Ok(()) => Ok(()),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            };
        }
        #[cfg(target_os = "windows")]
        {
            eprintln!("error: `intendant access` is not supported on Windows yet");
            std::process::exit(1);
        }
    }

    // Intercept `intendant peer <action>` before normal project/provider
    // initialization. Pairing creates or imports peer-issued mTLS client
    // identities and writes `[[peer]]` config; it should not need an API key.
    if env::args().nth(1).as_deref() == Some("peer") {
        let argv: Vec<String> = env::args().skip(2).collect();
        return match peer::pairing::run(argv).await {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        };
    }

    // Intercept `intendant setup <action>` before normal project/provider
    // initialization. These are host setup/repair commands and must not need
    // an API key or a detected project.
    if env::args().nth(1).as_deref() == Some("setup") {
        let argv: Vec<String> = env::args().skip(2).collect();
        return match setup::run(argv).await {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        };
    }

    // Intercept `intendant ctl <command>` before normal project/provider
    // initialization. The ctl namespace talks to a running daemon over MCP and
    // should stay a lightweight agent-facing control surface.
    if env::args().nth(1).as_deref() == Some("ctl") {
        let argv: Vec<String> = env::args().skip(2).collect();
        return match ctl::run(argv).await {
            Ok(()) => Ok(()),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        };
    }

    // Load .env: cwd (+ parents) first, then project root, then ~/.config/intendant/
    dotenvy::dotenv().ok();
    let mut project = Project::detect()?;
    dotenvy::from_path(project.root.join(".env")).ok();
    if let Some(config_dir) = dirs::config_dir() {
        dotenvy::from_path(config_dir.join("intendant").join(".env")).ok();
    }

    // Override env vars from CLI flags before provider selection
    let flags = parse_cli_flags()?;
    if let Some(ref p) = flags.provider {
        env::set_var("PROVIDER", p);
    }
    if let Some(ref m) = flags.model {
        env::set_var("MODEL_NAME", m);
    }
    // Apply project model config when CLI/env did not override.
    if env::var("MODEL_CONTEXT_WINDOW").is_err() {
        if let Some(ctx) = project.config.model.context_window {
            env::set_var("MODEL_CONTEXT_WINDOW", ctx.to_string());
        }
    }
    if env::var("MAX_OUTPUT_TOKENS").is_err() {
        if let Some(max_out) = project.config.model.max_output_tokens {
            env::set_var("MAX_OUTPUT_TOKENS", max_out.to_string());
        }
    }
    // Create or resume session log.
    //
    // Under the default-web daemon, --continue/--resume are owned by the
    // SUPERVISOR: the daemon starts on a fresh base log and resumes the
    // target session through ResumeSession at startup. (These flags used to
    // be silently swallowed — the daemon adopted the old session's log dir
    // and then idled.) The predicate mirrors use_web/web_daemon_requested
    // computed below; the flags it reads are not mutated in between.
    let daemon_owns_resume =
        should_start_idle_web_daemon(!flags.no_web && !flags.mcp && !flags.json_output, &flags)
            && (flags.continue_last || flags.resume_id.is_some());
    let mut daemon_startup_resume_dir: Option<PathBuf> = None;
    let log_dir = if let Some(ref session_id) = flags.resume_id {
        // --resume <id>: find a specific session by ID or path
        let dir = session_log::SessionLog::find_session_by_id(session_id).ok_or_else(|| {
            CallerError::Config(format!(
                "Resume requested, but session '{}' was not found",
                session_id
            ))
        })?;
        if daemon_owns_resume {
            daemon_startup_resume_dir = Some(dir);
            session_log::SessionLog::resolve_path(None)
        } else {
            dir
        }
    } else if flags.continue_last {
        // --continue: find the most recent session for this project
        let dir = session_log::SessionLog::find_latest_session(&project.root)
            .map(|(_, dir)| dir)
            .ok_or_else(|| {
                CallerError::Config(
                    "Continue requested, but no existing session was found for this project"
                        .to_string(),
                )
            })?;
        if daemon_owns_resume {
            daemon_startup_resume_dir = Some(dir);
            session_log::SessionLog::resolve_path(None)
        } else {
            dir
        }
    } else {
        session_log::SessionLog::resolve_path(flags.log_file.as_deref())
    };
    let session_log: SharedSessionLog = match session_log::SessionLog::open(log_dir.clone()) {
        Ok(log) => {
            eprintln!("Session log: {}/session.jsonl", log.dir().display());
            eprintln!("Session ID: {}", log.session_id());
            // Register session dir for the panic hook
            let _ = PANIC_LOG_DIR.set(log.dir().to_path_buf());
            Arc::new(Mutex::new(log))
        }
        Err(e) => {
            eprintln!(
                "Warning: Could not create session log at {}: {}",
                log_dir.display(),
                e
            );
            // Fallback to /tmp
            let fallback = PathBuf::from("/tmp/intendant_session");
            let log = session_log::SessionLog::open(fallback)
                .map_err(|e| CallerError::Config(format!("Cannot create session log: {}", e)))?;
            eprintln!(
                "Session log (fallback): {}/session.jsonl",
                log.dir().display()
            );
            Arc::new(Mutex::new(log))
        }
    };

    // Tee controller stderr/stdout into {session_dir}/daemon.log so the
    // "Download session report" button in Settings → Debug can include
    // controller-side output (eprintln!, panics, tracing) in the zip
    // alongside session.jsonl and turn files. Skipped when the
    // controller owns the real interactive TTY, because ratatui writes
    // escape sequences to stdout and cannot tolerate a pipe.
    {
        let will_use_web = !flags.no_web && !flags.mcp && !flags.json_output;
        let owns_real_tty = !will_use_web
            && !flags.no_tui
            && !flags.mcp
            && io::stdin().is_terminal()
            && io::stdout().is_terminal();
        if !owns_real_tty {
            let daemon_log_path = log_dir.join("daemon.log");
            if let Err(e) = daemon_log_tee::install(&daemon_log_path) {
                eprintln!(
                    "daemon_log_tee: could not tee stderr/stdout to {}: {}",
                    daemon_log_path.display(),
                    e
                );
            }
        }
    }

    // Create shared frame registry for video frame storage.
    let frame_registry: Arc<tokio::sync::RwLock<frames::FrameRegistry>> = Arc::new(
        tokio::sync::RwLock::new(frames::FrameRegistry::new(&log_dir)),
    );

    // Create recording registry (listener spawned after bus creation in each mode).
    if project.config.recording.enabled && !recording::is_ffmpeg_available() {
        slog(&session_log, |l| {
            l.warn("Recording enabled in intendant.toml but ffmpeg is not installed — recording will be disabled. Install with: sudo apt-get install ffmpeg")
        });
    }
    let recording_registry: Arc<tokio::sync::RwLock<recording::RecordingRegistry>> =
        Arc::new(tokio::sync::RwLock::new(recording::RecordingRegistry::new(
            &log_dir,
            project.config.recording.clone(),
        )));

    // Create shared display session registry (WebRTC display transport).
    let session_registry: display::SharedSessionRegistry =
        Arc::new(tokio::sync::RwLock::new(display::SessionRegistry::new()));

    configure_sandbox_env(&flags, &project, &log_dir);

    // --owner bootstrap: pin root authority to the given browser key
    // before any surface comes up. Failing this with the flag present is
    // fatal — an install whose only authority path silently failed would
    // be an unclaimable box.
    if let Some(owner) = flags.owner.as_deref() {
        let cert_dir = access::backend::select_backend().cert_dir();
        match access::iam::seed_owner_bootstrap_grant(&cert_dir, owner) {
            Ok(true) => eprintln!("[access] owner bootstrap: root grant pinned to client key"),
            Ok(false) => eprintln!("[access] owner bootstrap: client key already holds root"),
            Err(e) => {
                return Err(CallerError::Config(format!(
                    "--owner bootstrap failed: {e}"
                )));
            }
        }
    }

    // Credential custody: leases never survive a restart, so stale
    // materialized auth files (a crash's leftovers) are deleted before
    // anything can spawn an external agent; the timer keeps expiry
    // deleting materializations even when the lease store sees no calls.
    credential_leases::startup_materialization_sweep();
    tokio::spawn(async {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            credential_leases::sweep_now();
        }
    });

    // CLI --transcription flag overrides config file setting
    if flags.transcription {
        project.config.transcription.enabled = true;
    }

    // Install signal handler to mark session as interrupted before exit.
    // Rust's Drop trait does not run when the process is killed by a signal,
    // so we need an explicit handler to update session_meta.json. We catch
    // both SIGTERM (external shutdown) and SIGINT (Ctrl+C in terminal or at
    // the `run_daemon_loop` prompt after TUI quit) so the session doesn't
    // linger as `"status": "running"` in ~/.intendant/logs/ forever.
    {
        let signal_session_log = session_log.clone();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{signal, SignalKind};
                let mut sigterm =
                    signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
                let mut sigint =
                    signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
                tokio::select! {
                    _ = sigterm.recv() => {}
                    _ = sigint.recv() => {}
                }
                if let Ok(mut log) = signal_session_log.lock() {
                    log.mark_interrupted();
                }
                let interrupted_session_logs =
                    session_log::mark_registered_session_logs_interrupted_now();
                if !interrupted_session_logs.is_empty() {
                    eprintln!(
                        "Marked open session logs interrupted during signal shutdown: {:?}",
                        interrupted_session_logs
                    );
                }
                let cleaned_external_children =
                    external_agent::cleanup_spawned_child_processes_now();
                if !cleaned_external_children.is_empty() {
                    eprintln!(
                        "Cleaned up external-agent child processes during signal shutdown: {:?}",
                        cleaned_external_children
                    );
                }
                // Drop every credential lease (zeroizes memory, deletes
                // materialized oauth auth files) before the process dies.
                let _ = credential_leases::revoke(None, "daemon shutdown");
                // Clean up control socket
                control::cleanup();
                // Restore terminal (best-effort) so the shell isn't left in raw mode
                let _ = crossterm::terminal::disable_raw_mode();
                let _ = crossterm::execute!(
                    std::io::stdout(),
                    crossterm::terminal::LeaveAlternateScreen
                );
                std::process::exit(130);
            }
        });
    }

    // Write session metadata (project root, task will be filled in later if available).
    slog(&session_log, |l| {
        l.write_meta(Some(&project.root), None);
    });

    // Web gateway is on by default unless explicitly disabled, or when running
    // in MCP/JSON modes that own stdio.
    let use_web = !flags.no_web && !flags.mcp && !flags.json_output;
    let web_bind_ip = effective_web_bind_ip(&flags, &project.config.server);
    if use_web {
        validate_plaintext_web_bind(&flags, web_bind_ip)?;
    }

    // Resolve CLI/config external-agent choice once and share the effective
    // runtime value with dashboard APIs. This intentionally happens before
    // provider selection so `--agent codex` runs do not warn as if no worker is
    // available when only native provider API keys are missing.
    let initial_agent_backend =
        resolve_agent_backend_from_config(flags.agent_backend.clone(), &project);
    let shared_external_agent: Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>> =
        Arc::new(tokio::sync::RwLock::new(initial_agent_backend.clone()));
    let startup_external_resume_session = external_resume_session_for_startup(
        initial_agent_backend.as_ref(),
        &flags,
        session_log_id(&session_log).as_deref(),
    );
    let runtime_presence_enabled = !flags.no_presence && project.config.presence.enabled;

    // Resolve web port via auto-discovery, keeping the listener alive (no TOCTOU).
    let (web_port, mut web_listener) = if use_web {
        let (port, listener) = find_available_port(flags.web_port, web_bind_ip).await?;
        (port, Some(listener))
    } else {
        (flags.web_port, None)
    };
    // Only expose the web port to external agents when the web gateway is actually running.
    let web_port_for_agent: Option<u16> = if use_web { Some(web_port) } else { None };

    // Build the dashboard's TLS acceptor once (cheap to clone into each
    // gateway spawn site). Defaults to mTLS; `--no-tls` is the explicit
    // plaintext escape.
    // A misconfiguration (bad cert/key, half-specified pair) fails startup
    // here rather than silently degrading to plain HTTP. The bind address
    // feeds the self-signed cert's SAN list.
    let web_tls_client_cert_required = if use_web {
        web_mtls_enabled(
            &flags,
            &project.config.server.tls,
            &project.config.server.mtls,
        )
    } else {
        false
    };
    let web_tls_acceptor = if use_web {
        let bind_addr = web_listener.as_ref().and_then(|l| l.local_addr().ok());
        build_web_tls_acceptor(
            &flags,
            &project.config.server.tls,
            &project.config.server.mtls,
            bind_addr,
        )?
    } else {
        None
    };
    if web_tls_acceptor.is_some() {
        eprintln!(
            "[web_gateway] TLS enabled — dashboard is HTTPS/WSS-only on port {web_port} \
             (cleartext HTTP/WS connections are refused){}",
            if web_tls_client_cert_required {
                "; mTLS client certificates are required except for peer access and Connect bootstrap requests"
            } else {
                ""
            }
        );
    }

    let provider_result = provider::select_provider();
    let provider = match provider_result {
        Ok(p) => {
            slog(&session_log, |l| {
                l.debug(&format!("Provider: {}", p.name()));
                l.debug(&format!("Model: {}", p.model()));
            });
            Some(p)
        }
        Err(ref e) if use_web || flags.mcp || initial_agent_backend.is_some() => {
            // No API keys — this is not an error. External backends bring
            // their own authentication, and the dashboard's display control,
            // session browsing, annotations, and clipping all work without
            // inference. Keep the console note calm and free of error-shaped
            // text ("No API key found…") — automation reading stderr kept
            // mistaking that for a fatal startup failure. The full cause
            // stays in the session log.
            if let Some(backend) = &initial_agent_backend {
                eprintln!(
                    "Note: running without a native model provider — {} authenticates on its own. \
                     Native-model features (presence, sub-agents, voice) stay off until an API key is configured.",
                    backend
                );
            } else {
                eprintln!(
                    "Note: starting without a model provider — AI features stay off until an API key is configured. \
                     The dashboard, display control, and session browsing still work.",
                );
            }
            slog(&session_log, |l| {
                if let Some(backend) = &initial_agent_backend {
                    l.warn(&format!(
                        "No native model provider: {}; external agent configured: {}",
                        e, backend
                    ));
                } else {
                    l.warn(&format!("No AI provider: {}", e));
                }
            });
            None
        }
        Err(e) => return Err(e),
    };
    slog(&session_log, |l| {
        l.debug(&format!("Project root: {}", project.root.display()));
        l.debug(&format!("Autonomy: {}", flags.autonomy));
    });

    // Determine whether to use TUI (needed early for task resolution).
    // Idle web/dashboard startup defaults to the daemon path: no terminal TUI,
    // and the session supervisor owns all launches. `--no-web` keeps the
    // terminal TUI available for interactive local use.
    let web_daemon_requested = should_start_idle_web_daemon(use_web, &flags);
    let use_tui = !web_daemon_requested
        && (use_web
            || (!flags.no_tui
                && !flags.mcp
                && io::stdin().is_terminal()
                && io::stdout().is_terminal()));

    // Task resolution: MCP and TUI modes allow starting without a task.
    // MCP mode honors an explicit --task-file but must not otherwise call
    // get_task_from_flags_or_env() because it would print to stdout and read
    // from stdin, both reserved for JSON-RPC.
    // TUI mode can accept a task later via the follow-up input panel.
    // Headless mode still requires a task upfront.
    let task = resolve_initial_task_for_startup(&flags, web_daemon_requested, use_tui)?;

    if let Some(ref t) = task {
        slog(&session_log, |l| l.info(&format!("Task: {}", t)));
    }

    // Build autonomy state from project config + CLI flags
    let autonomy_state = AutonomyState::new(flags.autonomy, project.config.approval.clone());
    let autonomy = autonomy::shared_autonomy(autonomy_state);

    if web_daemon_requested {
        let bus = EventBus::new();
        let _tick_handle = event::spawn_tick_timer(bus.clone(), 1000);
        let _recording_listener = recording::spawn_recording_listener(
            bus.subscribe(),
            recording_registry.clone(),
            bus.clone(),
            Some(session_registry.clone()),
        );
        let _user_display_listener = spawn_user_display_listener(
            bus.clone(),
            session_registry.clone(),
            Some(frame_registry.clone()),
        );
        // Windows: auto-register the existing desktop as an active display so
        // the dashboard streams it on connect (mirrors the macOS end state of
        // a live session sitting in the registry). macOS/Linux compile this
        // out and keep activating only on an explicit grant.
        #[cfg(target_os = "windows")]
        auto_activate_windows_user_display(
            &bus,
            &session_registry,
            Some(frame_registry.clone()),
            &autonomy,
        )
        .await;
        start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
        let _debug_handler = Some(debug::spawn_debug_screen_handler(
            bus.subscribe(),
            project.config.recording.clone(),
            web_port,
            bus.clone(),
        ));

        let (outbound_tx, _) = tokio::sync::broadcast::channel::<String>(256);
        let _outbound_broadcaster =
            event::spawn_outbound_broadcaster(bus.subscribe(), outbound_tx.clone());
        let _session_log_writer =
            event::spawn_session_log_writer(bus.subscribe_session_log(), session_log.clone());
        let _fission_lifecycle_watcher = start_fission_lifecycle(&bus, &session_log);

        let (shared_file_watcher, _watcher_handle, _round_snapshot_handle) =
            if !file_watcher::root_is_snapshot_worthy(&project.root) {
                // Fallback roots (no .git / intendant.toml — e.g. a service's
                // $HOME WorkingDirectory) must never be baseline-scanned: it
                // blocks boot for minutes and shadow-copies the whole tree.
                eprintln!(
                    "[file_watcher] rewind snapshots off: {} is not a project root (no .git or \
                 intendant.toml) — start intendant inside a project to enable rewind",
                    project.root.display()
                );
                (None, None, None)
            } else {
                let snapshot_dir = log_dir.join("file_snapshots");
                match file_watcher::FileWatcher::new(
                    project.root.clone(),
                    snapshot_dir,
                    bus.clone(),
                ) {
                    Ok(watcher) => {
                        let (fw, wh, rh) = watcher.start_shared();
                        (Some(fw), Some(wh), Some(rh))
                    }
                    Err(e) => {
                        eprintln!("[file_watcher] Failed to start: {}", e);
                        (None, None, None)
                    }
                }
            };

        let transcriber: Option<std::sync::Arc<dyn transcription::Transcriber>> =
            if project.config.transcription.enabled {
                match transcription::WhisperTranscriber::new(&project.config.transcription) {
                    Ok(t) => Some(std::sync::Arc::new(t)),
                    Err(e) => {
                        eprintln!("Transcription init failed: {}", e);
                        None
                    }
                }
            } else {
                None
            };
        let mut web_config = web_gateway::build_config(
            project.config.presence.live_provider.as_deref(),
            project.config.presence.live_model.as_deref(),
            project.config.transcription.enabled,
            project.config.webrtc.to_ice_config(),
            project.config.webrtc.federation_allow_h264,
        );
        web_config.peer_access_requests = project.config.server.peer_access_requests.clone();
        web_config.connect = project.config.connect.clone().effective_with_env();
        web_config.presence_enabled = runtime_presence_enabled;
        web_config.external_agent = initial_agent_backend
            .as_ref()
            .map(|backend| backend.as_short_str().to_string());
        let shared_session = Arc::new(tokio::sync::RwLock::new(web_gateway::ActiveSessionState {
            daemon_session_id: session_log_id(&session_log),
            query_ctx: None,
            frame_registry: Some(frame_registry.clone()),
            session_log: None,
            recording_registry: Some(recording_registry.clone()),
            session_registry: Some(session_registry.clone()),
            snapshot_dir: Some(log_dir.join("file_snapshots")),
            project_root_for_changes: Some(project.root.clone()),
            runtime_settings: web_gateway::RuntimeSettingsState {
                external_agent: Some(shared_external_agent.clone()),
                presence_enabled: Some(runtime_presence_enabled),
            },
            file_watcher: shared_file_watcher.clone(),
        }));
        let mut mcp_http_state = mcp::McpAppState::new(
            "none".into(),
            "none".into(),
            autonomy.clone(),
            log_dir.clone(),
        );
        mcp_http_state.codex_managed_context =
            project::codex_managed_context_enabled(&project.config.agent.codex.managed_context);
        mcp_http_state.configured_codex_managed_context = mcp_http_state.codex_managed_context;
        mcp_http_state.frame_registry = Some(frame_registry.clone());
        mcp_http_state.session_registry = Some(session_registry.clone());
        mcp_http_state.screenshot_dir = Some(log_dir.join("screenshots"));
        let mcp_http_server = Some(Arc::new(mcp::IntendantServer::new_http(
            Arc::new(tokio::sync::RwLock::new(mcp_http_state)),
            bus.clone(),
        )));
        let peer_registry = build_and_hydrate_peer_registry(&log_dir, &project.config.peers);
        let advertise_urls = resolve_advertise_urls_from_flags_and_config(&flags, &project);
        let _web_handle = web_gateway::spawn_web_gateway(
            web_listener
                .take()
                .expect("web listener must exist when use_web"),
            bus.clone(),
            outbound_tx.clone(),
            web_config,
            shared_session.clone(),
            transcriber,
            None,
            None,
            Some(project.root.clone()),
            mcp_http_server,
            Some(peer_registry),
            advertise_urls,
            project.config.server.auth.bearer_token.clone(),
            build_local_advertised_auth(
                &project.config.server.auth,
                &access::backend::select_backend().cert_dir(),
            )?,
            web_tls_client_cert_required,
            web_tls_acceptor.clone(),
        );
        eprintln!(
            "{}",
            web_tui_log_line(&web_tls_acceptor, web_port, web_bind_ip)
        );

        let shared_codex_config: control_plane::SharedCodexConfig = {
            let cfg = &project.config.agent.codex;
            Arc::new(tokio::sync::RwLock::new(
                control_plane::CodexRuntimeConfig {
                    command: cfg.command.clone(),
                    managed_command: cfg.managed_command.clone(),
                    sandbox: project::normalize_sandbox_mode(&cfg.sandbox),
                    approval_policy: project::normalize_approval_policy(&cfg.approval_policy),
                    model: cfg.model.clone(),
                    reasoning_effort: project::normalize_reasoning_effort(
                        cfg.reasoning_effort.as_deref(),
                    ),
                    service_tier: project::normalize_codex_service_tier(
                        cfg.service_tier.as_deref(),
                    ),
                    web_search: cfg.web_search,
                    network_access: cfg.network_access,
                    writable_roots: cfg.writable_roots.clone(),
                    managed_context: project::normalize_codex_managed_context(&cfg.managed_context),
                    context_archive: project::normalize_codex_context_archive(&cfg.context_archive),
                },
            ))
        };
        let shared_claude_config = shared_claude_config_from_project(&project);
        let _control_plane_handle = control_plane::spawn(
            bus.subscribe(),
            control_plane::ControlPlaneState {
                autonomy: autonomy.clone(),
                external_agent: shared_external_agent.clone(),
                codex_config: shared_codex_config.clone(),
                claude_config: shared_claude_config.clone(),
                bus: bus.clone(),
                project_root: Some(project.root.clone()),
            },
        );

        let startup_bus = bus.clone();
        let supervisor_handle = session_supervisor::SessionSupervisor::new(
            session_supervisor::SessionSupervisorConfig {
                bus,
                project_root: project.root.clone(),
                autonomy,
                shared_external_agent,
                shared_codex_config,
                shared_claude_config,
                frame_registry,
                session_registry: Some(session_registry.clone()),
                web_port: web_port_for_agent,
                flags_direct: flags.direct,
                shared_session: Some(shared_session),
                provider_factory: None,
            },
        )
        .spawn();
        // --continue/--resume under the daemon: the supervisor (subscribed
        // above, before this send) resumes the target session — attach only,
        // no task; follow-ups come from the dashboard/TUI like any session.
        if let Some(resume_dir) = daemon_startup_resume_dir {
            let session_id = resume_dir
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            let source = crate::session_config::read_log_dir_config(&resume_dir)
                .and_then(|config| config.source)
                .unwrap_or_else(|| "intendant".to_string());
            eprintln!("Resuming session {session_id} ({source}) in the daemon");
            startup_bus.send(AppEvent::ControlCommand(event::ControlMsg::ResumeSession {
                source,
                session_id,
                resume_id: None,
                project_root: None,
                task: None,
                direct: None,
                attachments: Vec::new(),
                fork: false,
                agent_command: None,
                codex_sandbox: None,
                codex_approval_policy: None,
                codex_managed_context: None,
                codex_context_archive: None,
            }));
        }
        let _ = supervisor_handle.await;
        return Ok(());
    }

    if flags.mcp {
        // MCP mode — speaks Model Context Protocol on stdio.
        // This is architecturally a peer of the TUI: same EventBus, same UserAction contract.
        let bus = EventBus::new();
        let event_rx = bus.subscribe();
        let human_question_path = event::shared_question_path(log_dir.join("human_question"));
        let _human_monitor =
            event::spawn_human_question_monitor(bus.clone(), human_question_path.clone());
        let _tick_handle = event::spawn_tick_timer(bus.clone(), 1000);
        let _recording_listener = recording::spawn_recording_listener(
            bus.subscribe(),
            recording_registry.clone(),
            bus.clone(),
            Some(session_registry.clone()),
        );
        let _user_display_listener = spawn_user_display_listener(
            bus.clone(),
            session_registry.clone(),
            Some(frame_registry.clone()),
        );
        start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
        let _debug_handler = if use_web {
            Some(debug::spawn_debug_screen_handler(
                bus.subscribe(),
                project.config.recording.clone(),
                web_port,
                bus.clone(),
            ))
        } else {
            None
        };
        let mcp_control_tx = if flags.control_socket {
            let (_control_handle, control_tx) = control::spawn_control_server(bus.clone());
            slog(&session_log, |l| {
                l.info(&format!(
                    "Control socket: {}",
                    control::socket_path().display()
                ))
            });
            Some(control_tx)
        } else {
            None
        };

        // Outbound event broadcast channel — shared by control socket, web gateway,
        // and the outbound broadcaster.  If control socket is active, reuse its
        // channel; otherwise create a standalone one when web or broadcaster needs it.
        let outbound_tx = if let Some(ref tx) = mcp_control_tx {
            tx.clone()
        } else if use_web {
            let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
            tx
        } else {
            // No control socket, no web — create a channel anyway so the
            // outbound broadcaster can still run (receivers just drop events).
            let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
            tx
        };

        // Wire outbound broadcaster: converts AppEvents to OutboundEvents on the
        // shared broadcast channel.
        let _outbound_broadcaster =
            event::spawn_outbound_broadcaster(bus.subscribe(), outbound_tx.clone());

        // Wire session log writer: persists bus events that aren't logged inline.
        let _session_log_writer =
            event::spawn_session_log_writer(bus.subscribe_session_log(), session_log.clone());

        let _fission_lifecycle_watcher = start_fission_lifecycle(&bus, &session_log);

        // File watcher: observes project directory for changes, emits FileChanged events.
        let (shared_file_watcher, _watcher_handle, _round_snapshot_handle) =
            if !file_watcher::root_is_snapshot_worthy(&project.root) {
                // Fallback roots (no .git / intendant.toml — e.g. a service's
                // $HOME WorkingDirectory) must never be baseline-scanned: it
                // blocks boot for minutes and shadow-copies the whole tree.
                eprintln!(
                    "[file_watcher] rewind snapshots off: {} is not a project root (no .git or \
                 intendant.toml) — start intendant inside a project to enable rewind",
                    project.root.display()
                );
                (None, None, None)
            } else {
                let snapshot_dir = log_dir.join("file_snapshots");
                match file_watcher::FileWatcher::new(
                    project.root.clone(),
                    snapshot_dir,
                    bus.clone(),
                ) {
                    Ok(watcher) => {
                        let (fw, wh, rh) = watcher.start_shared();
                        (Some(fw), Some(wh), Some(rh))
                    }
                    Err(e) => {
                        eprintln!("[file_watcher] Failed to start: {}", e);
                        (None, None, None)
                    }
                }
            };

        // Web gateway (WebSocket)
        let _web_handle = if use_web {
            let broadcast_tx = outbound_tx.clone();
            let transcriber: Option<std::sync::Arc<dyn transcription::Transcriber>> =
                if project.config.transcription.enabled {
                    match transcription::WhisperTranscriber::new(&project.config.transcription) {
                        Ok(t) => Some(std::sync::Arc::new(t)),
                        Err(e) => {
                            eprintln!("Transcription init failed: {}", e);
                            None
                        }
                    }
                } else {
                    None
                };
            let mut config = web_gateway::build_config(
                project.config.presence.live_provider.as_deref(),
                project.config.presence.live_model.as_deref(),
                project.config.transcription.enabled,
                project.config.webrtc.to_ice_config(),
                project.config.webrtc.federation_allow_h264,
            );
            config.peer_access_requests = project.config.server.peer_access_requests.clone();
            config.connect = project.config.connect.clone().effective_with_env();
            config.presence_enabled = runtime_presence_enabled;
            config.external_agent = initial_agent_backend
                .as_ref()
                .map(|backend| backend.as_short_str().to_string());
            let snapshot_dir = log_dir.join("file_snapshots");
            let shared_session =
                Arc::new(tokio::sync::RwLock::new(web_gateway::ActiveSessionState {
                    daemon_session_id: session_log_id(&session_log),
                    query_ctx: None,
                    frame_registry: Some(frame_registry.clone()),
                    session_log: Some(session_log.clone()),
                    recording_registry: Some(recording_registry.clone()),
                    session_registry: Some(session_registry.clone()),
                    snapshot_dir: Some(snapshot_dir.clone()),
                    project_root_for_changes: Some(project.root.clone()),
                    runtime_settings: web_gateway::RuntimeSettingsState {
                        external_agent: Some(shared_external_agent.clone()),
                        presence_enabled: Some(runtime_presence_enabled),
                    },
                    file_watcher: shared_file_watcher.clone(),
                }));
            let mut mcp_http_state = mcp::McpAppState::new(
                "none".into(),
                "none".into(),
                autonomy.clone(),
                log_dir.clone(),
            );
            mcp_http_state.codex_managed_context =
                project::codex_managed_context_enabled(&project.config.agent.codex.managed_context);
            mcp_http_state.configured_codex_managed_context = mcp_http_state.codex_managed_context;
            mcp_http_state.frame_registry = Some(frame_registry.clone());
            mcp_http_state.session_registry = Some(session_registry.clone());
            mcp_http_state.screenshot_dir = Some(log_dir.join("screenshots"));
            let mcp_http_server = Some(Arc::new(mcp::IntendantServer::new_http(
                Arc::new(tokio::sync::RwLock::new(mcp_http_state)),
                bus.clone(),
            )));
            let peer_registry = build_and_hydrate_peer_registry(&log_dir, &project.config.peers);
            let advertise_urls = resolve_advertise_urls_from_flags_and_config(&flags, &project);
            let handle = web_gateway::spawn_web_gateway(
                web_listener
                    .take()
                    .expect("web listener must exist when use_web"),
                bus.clone(),
                broadcast_tx,
                config,
                shared_session,
                transcriber,
                None, // MCP mode: no WebTui
                None, // No task_tx in MCP mode
                Some(project.root.clone()),
                mcp_http_server,
                Some(peer_registry),
                advertise_urls,
                project.config.server.auth.bearer_token.clone(),
                build_local_advertised_auth(
                    &project.config.server.auth,
                    &access::backend::select_backend().cert_dir(),
                )?,
                web_tls_client_cert_required,
                web_tls_acceptor.clone(),
            );
            slog(&session_log, |l| {
                l.info(&web_tui_log_line(&web_tls_acceptor, web_port, web_bind_ip))
            });
            eprintln!(
                "{}",
                web_tui_log_line(&web_tls_acceptor, web_port, web_bind_ip)
            );
            Some(handle)
        } else {
            None
        };

        let mut mcp_app_state = mcp::McpAppState::new(
            provider
                .as_ref()
                .map(|p| p.name().to_string())
                .unwrap_or_else(|| "none".to_string()),
            provider
                .as_ref()
                .map(|p| p.model().to_string())
                .unwrap_or_else(|| "none".to_string()),
            autonomy.clone(),
            log_dir.clone(),
        );
        mcp_app_state.external_agent = initial_agent_backend.clone();
        mcp_app_state.codex_managed_context =
            project::codex_managed_context_enabled(&project.config.agent.codex.managed_context);
        mcp_app_state.configured_codex_managed_context = mcp_app_state.codex_managed_context;
        mcp_app_state.context_window = provider.as_ref().map(|p| p.context_window()).unwrap_or(0);
        mcp_app_state.hard_context_window = provider.as_ref().map(|p| p.context_window());
        mcp_app_state.session_id = session_log
            .lock()
            .map(|l| l.session_id().to_string())
            .unwrap_or_default();
        mcp_app_state.task_description = task.clone().unwrap_or_default();
        mcp_app_state.frame_registry = Some(frame_registry.clone());
        mcp_app_state.session_registry = Some(session_registry.clone());
        mcp_app_state.screenshot_dir = Some(log_dir.join("screenshots"));
        let mcp_state = std::sync::Arc::new(tokio::sync::RwLock::new(mcp_app_state));

        // Build a launcher closure that can spawn the agent loop on demand.
        // This captures the provider factory parameters (not the provider itself,
        // since providers are not Clone) so each start_task creates a fresh provider.
        let project_root = project.root.clone();
        let autonomy_for_launcher = autonomy.clone();
        let session_log_for_launcher = session_log.clone();
        let log_dir_for_launcher = log_dir.clone();
        let mcp_state_for_launcher = mcp_state.clone();
        let session_registry_for_launcher = session_registry.clone();
        #[allow(clippy::async_yields_async)]
        let launcher: mcp::TaskLauncher = Box::new(move |task_str: String, bus: EventBus| {
            let project_root = project_root.clone();
            let autonomy = autonomy_for_launcher.clone();
            let session_log = session_log_for_launcher.clone();
            let _parent_log_dir = log_dir_for_launcher.clone();
            let mcp_state = mcp_state_for_launcher.clone();
            let session_registry = session_registry_for_launcher.clone();
            Box::pin(async move {
                // Each MCP task gets a fresh session directory so conversations
                // don't bleed between tasks (reasoning items, tool calls, etc.).
                let task_log_dir = session_log::SessionLog::resolve_path(None);
                match session_log::SessionLog::open(task_log_dir.clone()) {
                    Ok(mut l) => {
                        l.write_meta(Some(&project_root), Some(&task_str));
                        l.info(&format!("MCP sub-task session: {}", l.session_id()));
                        // Replace the shared session log with the fresh one
                        if let Ok(mut guard) = session_log.lock() {
                            *guard = l;
                        }
                        // Notify MCP state of the new session dir so askHuman
                        // response files are written to the correct location.
                        bus.send(AppEvent::SessionDirChanged {
                            path: task_log_dir.clone(),
                        });
                    }
                    Err(e) => {
                        bus.send(AppEvent::LoopError(format!(
                            "Failed to create task session: {}",
                            e
                        )));
                        return tokio::spawn(async {});
                    }
                }
                let log_dir = task_log_dir;

                // Create a fresh provider for this task
                let provider = match provider::select_provider() {
                    Ok(p) => p,
                    Err(e) => {
                        bus.send(AppEvent::LoopError(format!(
                            "Failed to create provider: {}",
                            e
                        )));
                        return tokio::spawn(async {});
                    }
                };
                let project = match Project::from_root(project_root.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        bus.send(AppEvent::LoopError(format!(
                            "Failed to load project: {}",
                            e
                        )));
                        return tokio::spawn(async {});
                    }
                };
                // Consume the mode override set by start_task. Orchestration
                // (sub-agent spawning) needs the daemon's session supervisor;
                // this standalone MCP task path runs sessions directly, so an
                // orchestrate request degrades to a direct session.
                let orchestrate_override = {
                    let mut s = mcp_state.write().await;
                    s.next_task_orchestrate.take()
                };
                if orchestrate_override == Some(true) {
                    bus.send(AppEvent::LoopError(
                        "orchestrate=true requires the web daemon's session supervisor; \
                         running the task as a direct session"
                            .to_string(),
                    ));
                }

                // Create follow-up channel for multi-round support
                let (follow_up_tx, follow_up_rx) = tokio::sync::mpsc::channel::<FollowUpMessage>(1);
                {
                    let mut s = mcp_state.write().await;
                    s.follow_up_tx = Some(follow_up_tx);
                }

                let approval_registry = mcp_state.read().await.approval_registry.clone();
                let bus_clone = bus.clone();
                let task_for_summary = task_str.clone();
                let session_log_summary = session_log.clone();
                let mcp_state_cleanup = mcp_state.clone();
                // Resolve external agent backend: MCP shared state > config default
                let agent_backend = resolve_agent_backend_from_config(
                    mcp_state.read().await.external_agent.clone(),
                    &project,
                );

                tokio::spawn(async move {
                    let result = if let Some(backend) = agent_backend {
                        run_external_agent_mode(
                            backend,
                            task_str,
                            project,
                            bus_clone.clone(),
                            autonomy,
                            session_log,
                            log_dir,
                            follow_up_rx,
                            None,
                            approval_registry,
                            event::ContextInjectionQueue::default(),
                            false,
                            web_port_for_agent,
                            UserAttachments::default(),
                            None,
                            None,
                            None,
                            None,
                            false,
                            None,
                        )
                        .await
                    } else {
                        run_direct_mode(
                            provider,
                            task_str,
                            project,
                            bus_clone.clone(),
                            autonomy,
                            session_log,
                            log_dir,
                            None,
                            follow_up_rx,
                            None, // no JSON approval in MCP mode
                            approval_registry,
                            event::ContextInjectionQueue::default(),
                            Some(session_registry),
                            false, // not headless — MCP has interactive approval
                            UserAttachments::default(),
                            NativeSessionConfig::direct(),
                        )
                        .await
                    };

                    match result {
                        Ok(stats) => {
                            slog(&session_log_summary, |l| {
                                l.write_summary_with_rounds(
                                    &task_for_summary,
                                    "completed",
                                    stats.turns,
                                    Some(stats.rounds),
                                )
                            });
                            // Note: TaskComplete is already emitted by run_agent_loop
                            // when it breaks (done signal, no JSON, etc.)
                        }
                        Err(e) => {
                            slog(&session_log_summary, |l| {
                                l.write_summary(&task_for_summary, &format!("error: {}", e), 0)
                            });
                            bus_clone.send(AppEvent::LoopError(e.to_string()));
                        }
                    }

                    // Clean up follow-up sender so MCP knows no task is active
                    {
                        let mut s = mcp_state_cleanup.write().await;
                        s.follow_up_tx = None;
                    }
                })
            })
        });

        // Store the launcher in MCP state
        {
            let mut s = mcp_state.write().await;
            s.launcher = Some(std::sync::Arc::new(launcher));
        }

        // If a task was provided on the CLI, start it immediately
        if let Some(initial_task) = task {
            let handle = {
                let s = mcp_state.read().await;
                let launcher = s.launcher.as_ref().unwrap().clone();
                drop(s);
                (launcher)(initial_task, bus.clone()).await
            };
            let mut s = mcp_state.write().await;
            s.phase = types::Phase::Thinking;
            s.task_handle = Some(handle);
        }

        // Run the MCP server on stdio (blocks until client disconnects or quit)
        if let Err(e) = mcp::run_mcp_server(
            mcp_state,
            bus,
            event_rx,
            Some(human_question_path),
            mcp_control_tx,
        )
        .await
        {
            slog(&session_log, |l| {
                l.info(&format!("MCP server ended: {}", e))
            });
        }
        if flags.control_socket {
            control::cleanup();
        }
    } else if use_tui {
        // TUI mode — task may be None (user provides it via follow-up input)

        // TUI mode
        let bus = EventBus::new();
        let event_rx = bus.subscribe();

        // Spawn background tasks.
        // In web mode, key events come from WebSocket, not the terminal.
        let _crossterm_handle = if !use_web {
            Some(tui::event::spawn_crossterm_reader(bus.clone()))
        } else {
            None
        };
        let _tick_handle = event::spawn_tick_timer(bus.clone(), 100);
        let _human_monitor = event::spawn_human_question_monitor(
            bus.clone(),
            event::shared_question_path(log_dir.join("human_question")),
        );
        let _recording_listener = recording::spawn_recording_listener(
            bus.subscribe(),
            recording_registry.clone(),
            bus.clone(),
            Some(session_registry.clone()),
        );
        let _user_display_listener = spawn_user_display_listener(
            bus.clone(),
            session_registry.clone(),
            Some(frame_registry.clone()),
        );
        start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
        let _debug_handler = if use_web {
            Some(debug::spawn_debug_screen_handler(
                bus.subscribe(),
                project.config.recording.clone(),
                web_port,
                bus.clone(),
            ))
        } else {
            None
        };

        // TUI is created later — just before run() — so that web mode
        // (--web) can use WebTui instead of the real terminal backend.

        // Create app state
        let mut app = tui::app::App::new(
            provider
                .as_ref()
                .map(|p| p.name().to_string())
                .unwrap_or_else(|| "none".to_string()),
            provider
                .as_ref()
                .map(|p| p.model().to_string())
                .unwrap_or_else(|| "none".to_string()),
            autonomy.clone(),
            log_dir.clone(),
        );
        app.context_window = provider.as_ref().map(|p| p.context_window()).unwrap_or(0);
        app.session_id = session_log
            .lock()
            .map(|l| l.session_id().to_string())
            .unwrap_or_default();
        app.task_description = task.clone().unwrap_or_default();
        app.project_root = Some(project.root.clone());
        app.knowledge_path = Some(project.memory_path());
        app.skills = skills::discover_skills(Some(&project.root));
        if flags.verbose {
            app.pending_verbosity = Some(types::Verbosity::Debug);
        }
        if flags.control_socket {
            let (_control_handle, control_tx) = control::spawn_control_server(bus.clone());
            app.set_control_socket(control_tx);
            app.log(
                types::LogLevel::Info,
                format!("Control socket: {}", control::socket_path().display()),
            );
        }

        // Per-connection WebTui command channel (only for web mode).
        let (web_tui_tx, web_tui_rx) = if use_web {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<tui::web::WebTuiCommand>();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        // Web gateway broadcast channel — shares with control socket if both enabled.
        // The actual web gateway spawn is deferred until after presence setup so we
        // can pass the WebQueryCtx (agent state, project root, etc.) for tool requests.
        let web_broadcast_tx = if use_web {
            let tx = if let Some(ref tx) = app.control_tx {
                tx.clone()
            } else {
                let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
                app.set_control_socket(tx.clone());
                tx
            };
            Some(tx)
        } else {
            None
        };

        // Wire outbound broadcaster: converts AppEvents to OutboundEvents on the
        // shared broadcast channel (control socket / web gateway).
        let _outbound_broadcaster = app
            .control_tx
            .as_ref()
            .map(|tx| event::spawn_outbound_broadcaster(bus.subscribe(), tx.clone()));

        // Wire session log writer: persists bus events that aren't logged inline.
        let _session_log_writer =
            event::spawn_session_log_writer(bus.subscribe_session_log(), session_log.clone());

        let _fission_lifecycle_watcher = start_fission_lifecycle(&bus, &session_log);

        // File watcher: observes project directory for changes, emits FileChanged events.
        let (shared_file_watcher, _watcher_handle, _round_snapshot_handle) =
            if !file_watcher::root_is_snapshot_worthy(&project.root) {
                // Fallback roots (no .git / intendant.toml — e.g. a service's
                // $HOME WorkingDirectory) must never be baseline-scanned: it
                // blocks boot for minutes and shadow-copies the whole tree.
                eprintln!(
                    "[file_watcher] rewind snapshots off: {} is not a project root (no .git or \
                 intendant.toml) — start intendant inside a project to enable rewind",
                    project.root.display()
                );
                (None, None, None)
            } else {
                let snapshot_dir = log_dir.join("file_snapshots");
                match file_watcher::FileWatcher::new(
                    project.root.clone(),
                    snapshot_dir,
                    bus.clone(),
                ) {
                    Ok(watcher) => {
                        let (fw, wh, rh) = watcher.start_shared();
                        (Some(fw), Some(wh), Some(rh))
                    }
                    Err(e) => {
                        eprintln!("[file_watcher] Failed to start: {}", e);
                        (None, None, None)
                    }
                }
            };

        if let Some(ref t) = task {
            app.log(types::LogLevel::Info, format!("Task: {}", t));
        }

        // Determine if presence layer should be active.
        // Note: --direct only forces single-agent mode for the worker; it does
        // NOT disable presence.  Use --no-presence to disable presence.
        let use_presence = !flags.no_presence && project.config.presence.enabled;

        // Create follow-up channel for multi-round support.
        // When there is no initial task, the follow-up channel also delivers
        // the very first task from the input panel. Owned by the task
        // dispatcher (spawned below), not the TUI — the TUI emits
        // ControlCommand on the bus, the dispatcher routes.
        let (follow_up_tx, mut follow_up_rx) = tokio::sync::mpsc::channel::<FollowUpMessage>(4);

        // If no task was provided, start in follow-up mode so the user sees
        // the input panel immediately.
        if task.is_none() {
            app.current_phase = types::Phase::WaitingFollowUp;
            app.mode = tui::app::AppMode::FollowUp;
            let mut textarea = ratatui_textarea::TextArea::default();
            textarea.set_cursor_line_style(ratatui::style::Style::default());
            app.follow_up_textarea = Some(textarea);
            app.log(
                types::LogLevel::Info,
                "Ready. Enter a task to get started.".to_string(),
            );
        }

        // If presence is active, create channels for user ↔ presence communication
        // and the shared agent state snapshot. The presence_tx sender is owned by
        // the task dispatcher (spawned below), which routes non-direct user text
        // through the presence LLM.
        let (
            presence_user_rx,
            presence_event_rx_for_task,
            presence_agent_state,
            presence_tx_for_dispatch,
        ) = if use_presence {
            let (presence_tx, presence_user_rx) = tokio::sync::mpsc::channel::<String>(4);

            // Create presence event channel: TUI forwards filtered events here
            let (presence_event_tx, presence_event_rx) =
                tokio::sync::mpsc::channel::<presence::PresenceEvent>(64);
            app.set_presence_event_sender(presence_event_tx);

            // Shared agent state: updated by TUI (via forward_to_presence), read by presence tools
            let agent_state = Arc::new(std::sync::Mutex::new(
                presence::AgentStateSnapshot::default(),
            ));
            app.set_presence_agent_state(agent_state.clone());

            app.log_sourced(
                types::LogLevel::Info,
                "Presence layer active".to_string(),
                tui::app::LogSource::Presence,
                None,
            );
            // If there's an initial task, set the phase to Thinking immediately
            // so the TUI doesn't sit at "Idle" during the presence API call.
            if task.is_some() {
                app.current_phase = types::Phase::Thinking;
            }
            (
                Some(presence_user_rx),
                Some(presence_event_rx),
                Some(agent_state),
                Some(presence_tx),
            )
        } else {
            (None, None, None, None)
        };

        // Create the shared PresenceSession for event replay and checkpoints
        let presence_session = {
            let sid = session_log
                .lock()
                .map(|l| l.session_id().to_string())
                .unwrap_or_default();
            Arc::new(Mutex::new(presence::PresenceSession::new(sid)))
        };
        app.presence_session = Some(presence_session.clone());
        app.session_log = Some(session_log.clone());

        // Task dispatch channel: browser tool calls / dashboard StartTask →
        // presence task loop (CU-first routing). Only created when presence
        // is enabled, because the channel is consumed by `run_with_presence`.
        // The sender is owned by the dispatcher (spawned below) and by the
        // presence layer (its own `submit_task` tool). In non-presence mode,
        // leaving `task_tx` as None makes the dispatcher route to
        // `follow_up_tx` instead, which is consumed by
        // `run_external_agent_mode` / `run_direct_mode`.
        let (task_tx, task_rx) = if use_presence {
            let (tx, rx) = tokio::sync::mpsc::channel::<presence::TaskEnvelope>(4);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        // Spawn the backend task dispatcher. It listens on the bus for
        // ControlCommand(StartTask | FollowUp) and routes to the appropriate
        // channel. Replaces the routing logic that used to live in the TUI.
        let _dispatcher_handle = task_dispatch::Dispatcher {
            presence_tx: presence_tx_for_dispatch,
            task_tx: task_tx.clone(),
            follow_up_tx: Some(follow_up_tx.clone()),
            primary_session_id: session_log
                .lock()
                .map(|log| log.session_id().to_string())
                .ok(),
        }
        .spawn(bus.clone());

        // Deferred web gateway spawn — now we have the agent state for tool queries.
        // Note: WebQueryCtx is built UNCONDITIONALLY (not gated on presence).
        // The web dashboard's annotation Send button needs the context_injection
        // queue regardless of whether the presence layer is enabled, so that
        // injections still reach the agent loop in --no-presence mode.
        // When presence is disabled, agent_state is a fresh empty snapshot
        // (no live updates), but context_injection is still wired through.
        let mut web_shared_session_for_supervisor: Option<web_gateway::SharedActiveSession> = None;
        let _web_handle = if let Some(broadcast_tx) = web_broadcast_tx {
            let query_ctx_agent_state = presence_agent_state.clone().unwrap_or_else(|| {
                Arc::new(std::sync::Mutex::new(
                    presence::AgentStateSnapshot::default(),
                ))
            });
            let query_ctx = Some(web_gateway::WebQueryCtx {
                agent_state: query_ctx_agent_state,
                project_root: project.root.clone(),
                log_dir: log_dir.clone(),
                knowledge_path: project.memory_path(),
                presence_session: Some(presence_session.clone()),
                context_injection: Some(app.context_injection.clone()),
            });
            let transcriber: Option<std::sync::Arc<dyn transcription::Transcriber>> =
                if project.config.transcription.enabled {
                    match transcription::WhisperTranscriber::new(&project.config.transcription) {
                        Ok(t) => Some(std::sync::Arc::new(t)),
                        Err(e) => {
                            app.log(
                                types::LogLevel::Warn,
                                format!("Transcription init failed: {}", e),
                            );
                            None
                        }
                    }
                } else {
                    None
                };
            let mut config = web_gateway::build_config(
                project.config.presence.live_provider.as_deref(),
                project.config.presence.live_model.as_deref(),
                project.config.transcription.enabled,
                project.config.webrtc.to_ice_config(),
                project.config.webrtc.federation_allow_h264,
            );
            config.peer_access_requests = project.config.server.peer_access_requests.clone();
            config.connect = project.config.connect.clone().effective_with_env();
            config.presence_enabled = runtime_presence_enabled;
            config.external_agent = initial_agent_backend
                .as_ref()
                .map(|backend| backend.as_short_str().to_string());
            let snapshot_dir = log_dir.join("file_snapshots");
            let shared_session =
                Arc::new(tokio::sync::RwLock::new(web_gateway::ActiveSessionState {
                    daemon_session_id: session_log_id(&session_log),
                    query_ctx,
                    frame_registry: Some(frame_registry.clone()),
                    session_log: Some(session_log.clone()),
                    recording_registry: Some(recording_registry.clone()),
                    session_registry: Some(session_registry.clone()),
                    snapshot_dir: Some(snapshot_dir.clone()),
                    project_root_for_changes: Some(project.root.clone()),
                    runtime_settings: web_gateway::RuntimeSettingsState {
                        external_agent: Some(shared_external_agent.clone()),
                        presence_enabled: Some(runtime_presence_enabled),
                    },
                    file_watcher: shared_file_watcher.clone(),
                }));
            web_shared_session_for_supervisor = Some(shared_session.clone());
            // Create MCP server for HTTP transport (display/CU tools for external agents)
            let mut mcp_http_state = mcp::McpAppState::new(
                "none".into(),
                "none".into(),
                autonomy.clone(),
                log_dir.clone(),
            );
            mcp_http_state.codex_managed_context =
                project::codex_managed_context_enabled(&project.config.agent.codex.managed_context);
            mcp_http_state.configured_codex_managed_context = mcp_http_state.codex_managed_context;
            mcp_http_state.frame_registry = Some(frame_registry.clone());
            mcp_http_state.session_registry = Some(session_registry.clone());
            mcp_http_state.screenshot_dir = Some(log_dir.join("screenshots"));
            let mcp_http_server = Some(Arc::new(mcp::IntendantServer::new_http(
                Arc::new(tokio::sync::RwLock::new(mcp_http_state)),
                bus.clone(),
            )));
            let peer_registry = build_and_hydrate_peer_registry(&log_dir, &project.config.peers);
            // Browser-voice SubmitTask actions go via the EventBus → dispatcher
            // path (task_tx=None triggers the fallback at web_gateway.rs),
            // keeping a single routing authority.
            let advertise_urls = resolve_advertise_urls_from_flags_and_config(&flags, &project);
            let handle = web_gateway::spawn_web_gateway(
                web_listener
                    .take()
                    .expect("web listener must exist when use_web"),
                bus.clone(),
                broadcast_tx,
                config,
                shared_session,
                transcriber,
                web_tui_tx.clone(),
                None,
                Some(project.root.clone()),
                mcp_http_server,
                Some(peer_registry),
                advertise_urls,
                project.config.server.auth.bearer_token.clone(),
                build_local_advertised_auth(
                    &project.config.server.auth,
                    &access::backend::select_backend().cert_dir(),
                )?,
                web_tls_client_cert_required,
                web_tls_acceptor.clone(),
            );
            app.log(
                types::LogLevel::Info,
                web_tui_log_line(&web_tls_acceptor, web_port, web_bind_ip),
            );
            Some(handle)
        } else {
            None
        };

        // Save for daemon loop (project is moved into the agent loop closure)
        let project_root = project.root.clone();
        // Clone frame_registry for event handlers (original may be moved into spawns)
        let frame_registry_for_events = frame_registry.clone();

        // Spawn the agent loop in a background task
        let bus_clone = bus.clone();
        let autonomy_clone = autonomy.clone();
        let session_log_clone = session_log.clone();
        let session_log_summary = session_log.clone();
        let log_dir_clone = log_dir.clone();
        let approval_registry_clone = app.approval_registry.clone();
        let context_injection_clone = app.context_injection.clone();
        let session_registry_clone = session_registry.clone();
        let mcp_mgr = if !project.config.mcp_servers.is_empty() {
            Some(mcp_client::McpClientManager::connect_all(&project.config.mcp_servers).await)
        } else {
            None
        };
        let force_direct = flags.direct;
        // External agent backend resolved at startup; the shared runtime handle
        // above is kept in sync by ControlPlane SetExternalAgent messages.
        let agent_backend = initial_agent_backend.clone();
        // Live Codex config — seeded from TOML, updated by SetCodex* ControlMsgs.
        // The daemon loop reads this at the start of each task so a Control-tab
        // toggle takes effect on the next task without needing a restart.
        let shared_codex_config: control_plane::SharedCodexConfig = {
            let cfg = &project.config.agent.codex;
            Arc::new(tokio::sync::RwLock::new(
                control_plane::CodexRuntimeConfig {
                    command: cfg.command.clone(),
                    managed_command: cfg.managed_command.clone(),
                    sandbox: project::normalize_sandbox_mode(&cfg.sandbox),
                    approval_policy: project::normalize_approval_policy(&cfg.approval_policy),
                    model: cfg.model.clone(),
                    reasoning_effort: project::normalize_reasoning_effort(
                        cfg.reasoning_effort.as_deref(),
                    ),
                    service_tier: project::normalize_codex_service_tier(
                        cfg.service_tier.as_deref(),
                    ),
                    web_search: cfg.web_search,
                    network_access: cfg.network_access,
                    writable_roots: cfg.writable_roots.clone(),
                    managed_context: project::normalize_codex_managed_context(&cfg.managed_context),
                    context_archive: project::normalize_codex_context_archive(&cfg.context_archive),
                },
            ))
        };
        let shared_claude_config = shared_claude_config_from_project(&project);
        let _control_plane_handle = control_plane::spawn(
            bus.subscribe(),
            control_plane::ControlPlaneState {
                autonomy: autonomy.clone(),
                external_agent: shared_external_agent.clone(),
                codex_config: shared_codex_config.clone(),
                claude_config: shared_claude_config.clone(),
                bus: bus.clone(),
                project_root: Some(project.root.clone()),
            },
        );
        let _resume_listener_handle = if use_web {
            Some(
                session_supervisor::SessionSupervisor::new(
                    session_supervisor::SessionSupervisorConfig {
                        bus: bus.clone(),
                        project_root: project.root.clone(),
                        autonomy: autonomy.clone(),
                        shared_external_agent: shared_external_agent.clone(),
                        shared_codex_config: shared_codex_config.clone(),
                        shared_claude_config: shared_claude_config.clone(),
                        frame_registry: frame_registry.clone(),
                        session_registry: Some(session_registry.clone()),
                        web_port: web_port_for_agent,
                        flags_direct: flags.direct,
                        shared_session: web_shared_session_for_supervisor.clone(),
                        provider_factory: None,
                    },
                )
                .spawn_resume_listener(),
            )
        } else {
            None
        };
        // A startup `--resume`/`--continue` of an external session must run
        // with that session's persisted per-session agent config (managed
        // context, sandbox, approval policy, agent command, …), not the
        // global defaults — same rehydration the daemon resume path does in
        // `SessionSupervisor::resume_session`. Applied after the shared
        // runtime configs were seeded above so per-session overrides don't
        // leak into the dashboard's global Codex config.
        let startup_external_resume_overrides = agent_backend.as_ref().and_then(|backend| {
            apply_startup_external_resume_config(
                backend,
                &mut project,
                session_log_id(&session_log).as_deref(),
                startup_external_resume_session.as_deref(),
            )
        });

        let mut loop_handle = if use_presence {
            // Presence mode: the presence layer mediates between user and agent
            let presence_user_rx = presence_user_rx.unwrap();
            let presence_event_rx = presence_event_rx_for_task.unwrap();
            let agent_state = presence_agent_state.unwrap();
            // task_tx/task_rx are Some when use_presence is true (see above).
            let task_tx = task_tx.expect("task_tx created in presence mode");
            let task_rx = task_rx.expect("task_rx created in presence mode");
            let (response_tx, mut response_rx) = tokio::sync::mpsc::channel::<String>(8);

            // Shared paused ref-count: incremented by PresenceConnected, decremented by PresenceDisconnected.
            // Server-side presence is paused when count > 0 (any browser has active voice).
            let presence_paused = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            app.set_presence_paused_flag(presence_paused.clone());

            // Forward presence responses to TUI as log entries + reset phase
            let bus_for_responses = bus_clone.clone();
            let _response_forwarder = tokio::spawn(async move {
                while let Some(response) = response_rx.recv().await {
                    if !response.is_empty() {
                        if response.starts_with("Presence error:")
                            || response.starts_with("Presence provider timed out")
                        {
                            bus_for_responses.send(AppEvent::LoopError(response));
                        } else {
                            // Log presence response as a visible PresenceLog entry
                            bus_for_responses.send(AppEvent::PresenceLog {
                                message: format!("[presence] {}", response),
                                level: None,
                                turn: None,
                            });
                            // Switch to follow-up mode after presence responds
                            bus_for_responses.send(AppEvent::PresenceReady);
                        }
                    }
                }
            });

            let agent_backend_for_presence = agent_backend.clone();
            let shared_external_agent_for_presence = shared_external_agent.clone();
            let shared_codex_config_for_presence = shared_codex_config.clone();
            let shared_claude_config_for_presence = shared_claude_config.clone();
            let session_registry_for_presence = session_registry.clone();
            tokio::spawn(async move {
                let result = run_with_presence(
                    task,
                    project,
                    bus_clone.clone(),
                    autonomy_clone,
                    session_log_clone,
                    log_dir_clone,
                    presence_user_rx,
                    response_tx,
                    presence_event_rx,
                    agent_state,
                    force_direct,
                    presence_paused,
                    task_tx,
                    task_rx,
                    approval_registry_clone,
                    frame_registry.clone(),
                    context_injection_clone,
                    session_registry_for_presence,
                    agent_backend_for_presence,
                    shared_external_agent_for_presence,
                    shared_codex_config_for_presence,
                    shared_claude_config_for_presence,
                    if use_web { Some(web_port) } else { None },
                    startup_external_resume_session.clone(),
                    startup_external_resume_overrides,
                )
                .await;

                match result {
                    Ok(stats) => {
                        slog(&session_log_summary, |l| {
                            l.write_summary_with_rounds(
                                "(presence)",
                                "completed",
                                stats.turns,
                                Some(stats.rounds),
                            )
                        });
                    }
                    Err(e) => {
                        slog(&session_log_summary, |l| {
                            l.write_summary("(presence)", &format!("error: {}", e), 0)
                        });
                        bus_clone.send(AppEvent::LoopError(e.to_string()));
                    }
                }
            })
        } else {
            // Standard mode: direct agent loop.
            // When task is None, wait for the first follow-up message to
            // use as the task. This lets the TUI start idle.
            tokio::spawn(async move {
                let (task_str, follow_up_rx) = if let Some(t) = task {
                    (t, follow_up_rx)
                } else {
                    // Wait for the first message from the follow-up panel
                    match follow_up_rx.recv().await {
                        Some(first_task) => {
                            slog(&session_log_clone, |l| {
                                l.info(&format!("Task (from input): {}", first_task.text))
                            });
                            bus_clone.send(AppEvent::TurnStarted {
                                session_id: session_log_id(&session_log_clone),
                                turn: 0,
                                budget_pct: 0.0,
                                remaining: 0,
                            });
                            (first_task.text, follow_up_rx)
                        }
                        None => return, // channel closed before a task arrived
                    }
                };

                let result = if let Some(backend) = agent_backend {
                    run_external_agent_mode(
                        backend,
                        task_str,
                        project,
                        bus_clone.clone(),
                        autonomy_clone,
                        session_log_clone,
                        log_dir_clone,
                        follow_up_rx,
                        None,
                        approval_registry_clone,
                        context_injection_clone.clone(),
                        false, // not headless — TUI handles approval
                        web_port_for_agent,
                        UserAttachments::default(),
                        startup_external_resume_session.clone(),
                        startup_external_resume_overrides
                            .as_ref()
                            .and_then(|config| config.codex_service_tier.clone()),
                        startup_external_resume_overrides
                            .as_ref()
                            .and_then(|config| config.codex_home.clone()),
                        None,
                        false,
                        None,
                    )
                    .await
                } else {
                    // Re-select provider at task start (may have been None at startup)
                    let provider = match provider.or_else(|| provider::select_provider().ok()) {
                        Some(p) => p,
                        None => {
                            bus_clone.send(AppEvent::LoopError(
                                "No API key configured. Set OPENAI_API_KEY, ANTHROPIC_API_KEY, or GEMINI_API_KEY.".to_string()
                            ));
                            return;
                        }
                    };

                    // Orchestration (sub-agent spawning) requires the
                    // daemon's session supervisor; TUI-mode tasks run as
                    // direct sessions.
                    run_direct_mode(
                        provider,
                        task_str,
                        project,
                        bus_clone.clone(),
                        autonomy_clone,
                        session_log_clone,
                        log_dir_clone,
                        mcp_mgr,
                        follow_up_rx,
                        None, // no JSON approval in TUI mode
                        approval_registry_clone,
                        context_injection_clone,
                        Some(session_registry_clone),
                        false, // not headless — TUI handles approval
                        UserAttachments::default(),
                        NativeSessionConfig::direct(),
                    )
                    .await
                };

                match result {
                    Ok(stats) => {
                        slog(&session_log_summary, |l| {
                            l.write_summary_with_rounds(
                                "(tui)",
                                "completed",
                                stats.turns,
                                Some(stats.rounds),
                            )
                        });
                    }
                    Err(e) => {
                        slog(&session_log_summary, |l| {
                            l.write_summary("(tui)", &format!("error: {}", e), 0)
                        });
                        bus_clone.send(AppEvent::LoopError(e.to_string()));
                    }
                }
            })
        };

        // Run the TUI event loop (blocks until quit).
        // In web mode, render to a buffer and stream to xterm.js.
        // In terminal mode, render directly to stdout.
        if use_web {
            let broadcast_tx = app.control_tx.clone().unwrap_or_else(|| {
                let (tx, _) = tokio::sync::broadcast::channel::<String>(256);
                app.set_control_socket(tx.clone());
                tx
            });
            eprintln!(
                "{}",
                web_tui_log_line(&web_tls_acceptor, web_port, web_bind_ip)
            );
            let mut web_tui = tui::web::WebTui::new(120, 40, broadcast_tx)
                .map_err(|e| CallerError::Tui(format!("Failed to initialize Web TUI: {}", e)))?;
            let cmd_rx = web_tui_rx.expect("web_tui_rx must exist in web mode");
            let _ = web_tui.run(&mut app, event_rx, cmd_rx, bus.clone()).await;
        } else {
            let mut terminal = tui::Tui::new()
                .map_err(|e| CallerError::Tui(format!("Failed to initialize TUI: {}", e)))?;
            let _ = terminal.run(&mut app, event_rx, bus.clone()).await;
        }

        // Drop the App (and its follow_up_tx) so the round loop's recv()
        // returns None and exits gracefully, allowing write_summary to run.
        let session_id = app.session_id.clone();
        drop(app);

        // Give the agent task a moment to finish writing the session summary.
        // If it doesn't finish in time (e.g. stuck on an API call), abort it.
        match tokio::time::timeout(std::time::Duration::from_secs(5), &mut loop_handle).await {
            Ok(_) => {}                    // task finished naturally
            Err(_) => loop_handle.abort(), // timed out — force stop
        }

        if use_web && !session_id.is_empty() {
            bus.send(AppEvent::SessionEnded {
                session_id,
                reason: "completed".to_string(),
            });
            // Daemon mode: keep web gateway alive after TUI quits.
            // Fall through to a headless daemon loop (TUI is not re-created).
            eprintln!(
                "TUI exited. Web gateway still running on port {}. Waiting for new tasks...",
                web_port
            );
            run_daemon_loop(DaemonConfig {
                bus: bus.clone(),
                project_root: project_root.clone(),
                autonomy: autonomy.clone(),
                shared_external_agent: shared_external_agent.clone(),
                shared_codex_config: shared_codex_config.clone(),
                shared_claude_config: shared_claude_config.clone(),
                frame_registry: frame_registry_for_events.clone(),
                session_registry: Some(session_registry.clone()),
                web_port: web_port_for_agent,
                flags_direct: flags.direct,
                shared_session: None,
            })
            .await;
        }

        control::cleanup();
    } else {
        // Headless mode always has a task (enforced above).
        let task = task.unwrap();

        // Headless mode: no WebTui or terminal TUI is active.
        let bus = EventBus::new();
        let _recording_listener = recording::spawn_recording_listener(
            bus.subscribe(),
            recording_registry.clone(),
            bus.clone(),
            Some(session_registry.clone()),
        );
        let _user_display_listener = spawn_user_display_listener(
            bus.clone(),
            session_registry.clone(),
            Some(frame_registry.clone()),
        );
        start_external_display_recordings(&flags.record_displays, &recording_registry, &bus).await;
        let _debug_handler = if use_web {
            Some(debug::spawn_debug_screen_handler(
                bus.subscribe(),
                project.config.recording.clone(),
                web_port,
                bus.clone(),
            ))
        } else {
            None
        };

        // Outbound broadcast channel — shared by web gateway and JSON stdout subscriber
        let (outbound_tx, _) = tokio::sync::broadcast::channel::<String>(256);

        // Wire outbound broadcaster: converts AppEvents to OutboundEvents
        let _outbound_broadcaster =
            event::spawn_outbound_broadcaster(bus.subscribe(), outbound_tx.clone());

        // Wire session log writer: persists bus events that aren't logged inline.
        let _session_log_writer =
            event::spawn_session_log_writer(bus.subscribe_session_log(), session_log.clone());

        let _fission_lifecycle_watcher = start_fission_lifecycle(&bus, &session_log);

        // File watcher: observes project directory for changes, emits FileChanged events.
        let (shared_file_watcher, _watcher_handle, _round_snapshot_handle) =
            if !file_watcher::root_is_snapshot_worthy(&project.root) {
                // Fallback roots (no .git / intendant.toml — e.g. a service's
                // $HOME WorkingDirectory) must never be baseline-scanned: it
                // blocks boot for minutes and shadow-copies the whole tree.
                eprintln!(
                    "[file_watcher] rewind snapshots off: {} is not a project root (no .git or \
                 intendant.toml) — start intendant inside a project to enable rewind",
                    project.root.display()
                );
                (None, None, None)
            } else {
                let snapshot_dir = log_dir.join("file_snapshots");
                match file_watcher::FileWatcher::new(
                    project.root.clone(),
                    snapshot_dir,
                    bus.clone(),
                ) {
                    Ok(watcher) => {
                        let (fw, wh, rh) = watcher.start_shared();
                        (Some(fw), Some(wh), Some(rh))
                    }
                    Err(e) => {
                        eprintln!("[file_watcher] Failed to start: {}", e);
                        (None, None, None)
                    }
                }
            };

        // JSON stdout subscriber: prints OutboundEvents as JSONL to stdout
        if flags.json_output {
            let mut json_rx = outbound_tx.subscribe();
            tokio::spawn(async move {
                loop {
                    match json_rx.recv().await {
                        Ok(line) => {
                            println!("{}", line);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        // Web gateway in headless mode
        let headless_shared_session: Option<web_gateway::SharedActiveSession> = if use_web {
            let transcriber: Option<std::sync::Arc<dyn transcription::Transcriber>> =
                if project.config.transcription.enabled {
                    match transcription::WhisperTranscriber::new(&project.config.transcription) {
                        Ok(t) => Some(std::sync::Arc::new(t)),
                        Err(e) => {
                            eprintln!("Transcription init failed: {}", e);
                            None
                        }
                    }
                } else {
                    None
                };
            let mut config = web_gateway::build_config(
                project.config.presence.live_provider.as_deref(),
                project.config.presence.live_model.as_deref(),
                project.config.transcription.enabled,
                project.config.webrtc.to_ice_config(),
                project.config.webrtc.federation_allow_h264,
            );
            config.peer_access_requests = project.config.server.peer_access_requests.clone();
            config.connect = project.config.connect.clone().effective_with_env();
            config.presence_enabled = runtime_presence_enabled;
            config.external_agent = initial_agent_backend
                .as_ref()
                .map(|backend| backend.as_short_str().to_string());
            let snapshot_dir = log_dir.join("file_snapshots");
            let shared_session =
                Arc::new(tokio::sync::RwLock::new(web_gateway::ActiveSessionState {
                    daemon_session_id: session_log_id(&session_log),
                    query_ctx: None,
                    frame_registry: Some(frame_registry.clone()),
                    session_log: Some(session_log.clone()),
                    recording_registry: Some(recording_registry.clone()),
                    session_registry: Some(session_registry.clone()),
                    snapshot_dir: Some(snapshot_dir.clone()),
                    project_root_for_changes: Some(project.root.clone()),
                    runtime_settings: web_gateway::RuntimeSettingsState {
                        external_agent: Some(shared_external_agent.clone()),
                        presence_enabled: Some(runtime_presence_enabled),
                    },
                    file_watcher: shared_file_watcher.clone(),
                }));
            let mut mcp_http_state = mcp::McpAppState::new(
                "none".into(),
                "none".into(),
                autonomy.clone(),
                log_dir.clone(),
            );
            mcp_http_state.codex_managed_context =
                project::codex_managed_context_enabled(&project.config.agent.codex.managed_context);
            mcp_http_state.configured_codex_managed_context = mcp_http_state.codex_managed_context;
            mcp_http_state.frame_registry = Some(frame_registry.clone());
            mcp_http_state.session_registry = Some(session_registry.clone());
            mcp_http_state.screenshot_dir = Some(log_dir.join("screenshots"));
            let mcp_http_server = Some(Arc::new(mcp::IntendantServer::new_http(
                Arc::new(tokio::sync::RwLock::new(mcp_http_state)),
                bus.clone(),
            )));
            let peer_registry = build_and_hydrate_peer_registry(&log_dir, &project.config.peers);
            let advertise_urls = resolve_advertise_urls_from_flags_and_config(&flags, &project);
            let _web_handle = web_gateway::spawn_web_gateway(
                web_listener
                    .take()
                    .expect("web listener must exist when use_web"),
                bus.clone(),
                outbound_tx.clone(),
                config,
                shared_session.clone(),
                transcriber,
                None, // Headless mode: no WebTui
                None, // No task_tx in headless mode
                Some(project.root.clone()),
                mcp_http_server,
                Some(peer_registry),
                advertise_urls,
                project.config.server.auth.bearer_token.clone(),
                build_local_advertised_auth(
                    &project.config.server.auth,
                    &access::backend::select_backend().cert_dir(),
                )?,
                web_tls_client_cert_required,
                web_tls_acceptor.clone(),
            );
            eprintln!(
                "{}",
                web_tui_log_line(&web_tls_acceptor, web_port, web_bind_ip)
            );
            Some(shared_session)
        } else {
            None
        };

        let mcp_mgr = if !project.config.mcp_servers.is_empty() {
            Some(mcp_client::McpClientManager::connect_all(&project.config.mcp_servers).await)
        } else {
            None
        };

        // Create follow-up channel. In JSON mode, spawn a stdin reader to enable
        // follow-up via stdin lines and JSON commands (approve, deny, input, etc.).
        // Otherwise, drop the sender immediately so recv() returns None → single-round.
        let (follow_up_tx, follow_up_rx) = tokio::sync::mpsc::channel::<FollowUpMessage>(1);
        let json_approval_slot = if flags.json_output {
            Some(new_json_approval_slot())
        } else {
            None
        };
        if flags.json_output {
            // JSON mode: read follow-up lines and control commands from stdin
            let approval_slot = json_approval_slot.clone().unwrap();
            let log_dir_for_stdin = log_dir.clone();
            tokio::spawn(async move {
                let stdin = tokio::io::stdin();
                let reader = tokio::io::BufReader::new(stdin);
                use tokio::io::AsyncBufReadExt;
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    // Try to parse as a JSON control command
                    if line.starts_with('{') {
                        if let Ok(msg) = serde_json::from_str::<event::ControlMsg>(&line) {
                            match msg {
                                event::ControlMsg::Approve { .. } => {
                                    let mut guard = approval_slot.lock().unwrap();
                                    if let Some((_id, tx)) = guard.take() {
                                        let _ = tx.send(event::ApprovalResponse::Approve);
                                    }
                                }
                                event::ControlMsg::Deny { .. } => {
                                    let mut guard = approval_slot.lock().unwrap();
                                    if let Some((_id, tx)) = guard.take() {
                                        let _ = tx.send(event::ApprovalResponse::Deny);
                                    }
                                }
                                event::ControlMsg::Skip { .. } => {
                                    let mut guard = approval_slot.lock().unwrap();
                                    if let Some((_id, tx)) = guard.take() {
                                        let _ = tx.send(event::ApprovalResponse::Skip);
                                    }
                                }
                                event::ControlMsg::ApproveAll { .. } => {
                                    let mut guard = approval_slot.lock().unwrap();
                                    if let Some((_id, tx)) = guard.take() {
                                        let _ = tx.send(event::ApprovalResponse::ApproveAll);
                                    }
                                }
                                event::ControlMsg::Input { text } => {
                                    // Write human_response file for askHuman IPC.
                                    // The agent polls for this file; a swallowed
                                    // failure leaves it waiting forever.
                                    let resp_path = log_dir_for_stdin.join("human_response");
                                    if let Err(e) = std::fs::write(&resp_path, text.as_bytes()) {
                                        eprintln!(
                                            "Failed to write askHuman response {}: {}",
                                            resp_path.display(),
                                            e
                                        );
                                    }
                                }
                                event::ControlMsg::FollowUp {
                                    text, direct: _, ..
                                } => {
                                    // This stdin handler only exists in
                                    // the headless `--json` path where
                                    // there's no presence layer, so the
                                    // direct bit is implicitly always on.
                                    if follow_up_tx
                                        .send(FollowUpMessage::text(text))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                                _ => {
                                    // Unknown command — ignore
                                }
                            }
                            continue;
                        }
                    }
                    // Plain text → follow-up message
                    if follow_up_tx
                        .send(FollowUpMessage::text(line))
                        .await
                        .is_err()
                    {
                        break; // receiver dropped
                    }
                }
            });
        } else {
            drop(follow_up_tx); // single-round: recv() returns None immediately
        }

        let session_id = log_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        bus.send(AppEvent::SessionStarted {
            session_id: session_id.clone(),
            task: Some(task.clone()),
        });

        // Save for daemon loop (project and autonomy are moved into the agent loop)
        let project_root = project.root.clone();
        let autonomy_for_daemon = autonomy.clone();

        // External agent backend resolved at startup; the shared runtime handle
        // above is kept in sync by ControlPlane SetExternalAgent messages.
        let agent_backend = initial_agent_backend.clone();
        let shared_codex_config: control_plane::SharedCodexConfig = {
            let cfg = &project.config.agent.codex;
            Arc::new(tokio::sync::RwLock::new(
                control_plane::CodexRuntimeConfig {
                    command: cfg.command.clone(),
                    managed_command: cfg.managed_command.clone(),
                    sandbox: project::normalize_sandbox_mode(&cfg.sandbox),
                    approval_policy: project::normalize_approval_policy(&cfg.approval_policy),
                    model: cfg.model.clone(),
                    reasoning_effort: project::normalize_reasoning_effort(
                        cfg.reasoning_effort.as_deref(),
                    ),
                    service_tier: project::normalize_codex_service_tier(
                        cfg.service_tier.as_deref(),
                    ),
                    web_search: cfg.web_search,
                    network_access: cfg.network_access,
                    writable_roots: cfg.writable_roots.clone(),
                    managed_context: project::normalize_codex_managed_context(&cfg.managed_context),
                    context_archive: project::normalize_codex_context_archive(&cfg.context_archive),
                },
            ))
        };
        let shared_claude_config = shared_claude_config_from_project(&project);
        let _control_plane_handle = control_plane::spawn(
            bus.subscribe(),
            control_plane::ControlPlaneState {
                autonomy: autonomy.clone(),
                external_agent: shared_external_agent.clone(),
                codex_config: shared_codex_config.clone(),
                claude_config: shared_claude_config.clone(),
                bus: bus.clone(),
                project_root: Some(project.root.clone()),
            },
        );

        // Rehydrate the resumed session's persisted per-session agent config
        // (managed context, sandbox, approval policy, agent command, …) for a
        // startup `--resume`/`--continue`, mirroring the daemon resume path in
        // `SessionSupervisor::resume_session`. Applied after the shared runtime
        // configs were seeded above so per-session overrides stay per-session.
        let startup_external_resume_overrides = agent_backend.as_ref().and_then(|backend| {
            apply_startup_external_resume_config(
                backend,
                &mut project,
                session_log_id(&session_log).as_deref(),
                startup_external_resume_session.as_deref(),
            )
        });

        let result = if let Some(backend) = agent_backend {
            run_external_agent_mode(
                backend,
                task.clone(),
                project,
                bus.clone(),
                autonomy,
                session_log.clone(),
                log_dir,
                follow_up_rx,
                json_approval_slot,
                event::ApprovalRegistry::default(),
                event::ContextInjectionQueue::default(),
                true, // headless mode
                web_port_for_agent,
                UserAttachments::default(),
                startup_external_resume_session.clone(),
                startup_external_resume_overrides
                    .as_ref()
                    .and_then(|config| config.codex_service_tier.clone()),
                startup_external_resume_overrides
                    .as_ref()
                    .and_then(|config| config.codex_home.clone()),
                None,
                false,
                None,
            )
            .await
        } else {
            let provider = provider.ok_or_else(|| {
                CallerError::Config("Headless mode requires an API key".to_string())
            })?;
            // Orchestration (sub-agent spawning) requires the daemon's
            // session supervisor; headless non-daemon tasks run as direct
            // sessions.
            run_direct_mode(
                provider,
                task.clone(),
                project,
                bus.clone(),
                autonomy,
                session_log.clone(),
                log_dir,
                mcp_mgr,
                follow_up_rx,
                json_approval_slot,
                event::ApprovalRegistry::default(),
                event::ContextInjectionQueue::default(),
                Some(session_registry.clone()),
                true, // headless mode
                UserAttachments::default(),
                NativeSessionConfig::direct(),
            )
            .await
        };

        let reason = match &result {
            Ok(stats) => {
                let outcome = stats.terminal_outcome.as_deref().unwrap_or("completed");
                slog(&session_log, |l| {
                    l.write_summary_with_rounds(&task, outcome, stats.turns, Some(stats.rounds))
                });
                outcome.to_string()
            }
            Err(e) => {
                slog(&session_log, |l| {
                    l.write_summary(&task, &format!("error: {}", e), 0)
                });
                format!("error: {}", e)
            }
        };

        bus.send(AppEvent::SessionEnded {
            session_id,
            reason: reason.clone(),
        });

        if use_web {
            // Daemon mode: keep web gateway alive, listen for new tasks from web UI.
            if let Some(ref shared_session) = headless_shared_session {
                // Clear session-specific state so new connections see "no active session"
                {
                    let mut ss = shared_session.write().await;
                    ss.query_ctx = None;
                    ss.session_log = None;
                    // Keep frame_registry and recording_registry alive
                }
            }
            eprintln!(
                "Session ended ({}). Web gateway running on port {}. Waiting for new tasks...",
                reason, web_port
            );

            run_daemon_loop(DaemonConfig {
                bus: bus.clone(),
                project_root: project_root.clone(),
                autonomy: autonomy_for_daemon.clone(),
                shared_external_agent: shared_external_agent.clone(),
                shared_codex_config: shared_codex_config.clone(),
                shared_claude_config: shared_claude_config.clone(),
                frame_registry: frame_registry.clone(),
                session_registry: Some(session_registry.clone()),
                web_port: web_port_for_agent,
                flags_direct: flags.direct,
                shared_session: headless_shared_session.clone(),
            })
            .await;
        } else {
            result?;
        }
    }

    Ok(())
}
