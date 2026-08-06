//! External-agent supervision helpers: startup resume config,
//! session identity and round bookkeeping, event targeting for
//! external sessions and side threads, unified-diff tracking for the
//! diff panel, backend resolution, and external-agent construction
//! (create_external_agent, DrainConfig, snapshot/recovery state).

// Same entangled class as the drain (external_events.rs): keeps the
// crate-root view it was written against. Narrowing to named imports
// is the deferred cosmetic pass (see the god-file split design).
use crate::*;

pub(crate) fn external_resume_session_for_startup(
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

pub(crate) fn external_resume_session_for_startup_in_home(
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
pub(crate) fn apply_startup_external_resume_config(
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

pub(crate) fn apply_startup_external_resume_config_in_home(
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

pub(crate) fn emit_external_session_identity(
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

pub(crate) fn record_external_done_and_round_inline(
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

pub(crate) fn record_external_round_inline(
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

pub(crate) fn external_rollback_turn_in_progress(err: &CallerError) -> bool {
    let CallerError::ExternalAgent(message) = err else {
        return false;
    };
    message
        .to_ascii_lowercase()
        .contains("cannot rollback while a turn is in progress")
}

pub(crate) fn event_targets_session_or_alias(
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
pub(crate) fn rotate_external_identity(
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

pub(crate) fn event_targets_external_session_or_side(
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

pub(crate) fn event_targets_external_session_or_optional_side(
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

/// Non-blocking peek at a persistent external agent's event channel: returns
/// a buffered event if one is already waiting, and disables the receiver
/// (sets it to `None`) when the reader task is gone so the caller's select
/// arm logic stays consistent with a `recv() -> None`.
///
/// Used by the idle queued-steer flush: a buffered event means the backend
/// is (or is about to be) mid-turn — e.g. Claude Code starting a spontaneous
/// task-notification round — and CC 2.1.2xx discards stdin written mid-turn,
/// so flushing first would emit `SteerDelivered` for text the model never
/// saw. Processing the buffered event first routes a turn start through the
/// spontaneous-round drain, which delivers queued steers at a real boundary.
pub(crate) fn try_buffered_idle_agent_event(
    event_rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>>,
) -> Option<external_agent::AgentEvent> {
    let rx = event_rx.as_mut()?;
    match rx.try_recv() {
        Ok(event) => Some(event),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => None,
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
            *event_rx = None;
            None
        }
    }
}

/// Emit one canonical user-message row: a session-log `[user]` line that
/// persists the turn metadata + renderable attachment refs in `data`, and
/// the live `UserMessageLog` bus event carrying the same fields — so the
/// live wire row and its replayed copy are identically tagged and the
/// dashboard's transcript-signature dedupe collapses them across lanes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_user_message_log(
    bus: &EventBus,
    session_log: &SharedSessionLog,
    session_id: Option<&str>,
    user_turn_index: Option<u32>,
    user_turn_revision: Option<u32>,
    replacement_for_user_turn_index: Option<u32>,
    attachments: &[crate::types::SessionNoteAttachment],
    text: &str,
) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    slog(session_log, |l| {
        l.user_message(
            text,
            user_turn_index,
            user_turn_revision,
            replacement_for_user_turn_index,
            attachments,
        )
    });
    bus.send(AppEvent::UserMessageLog {
        session_id: session_id.map(str::to_string),
        content: text.to_string(),
        user_turn_index,
        user_turn_revision,
        replacement_for_user_turn_index,
        attachments: attachments.to_vec(),
    });
}

pub(crate) fn emit_external_session_loop_error(
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

/// Resolve external agent backend from an explicit override, falling back to
/// the project config's `agent.default_backend` setting.
pub(crate) fn resolve_agent_backend_from_config(
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
pub(crate) fn codex_runtime_config_equal(
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

pub(crate) fn claude_runtime_config_equal(
    a: &control_plane::ClaudeRuntimeConfig,
    b: &control_plane::ClaudeRuntimeConfig,
) -> bool {
    a.model == b.model
        && a.effort == b.effort
        && a.permission_mode == b.permission_mode
        && a.allowed_tools == b.allowed_tools
}

pub(crate) fn kimi_runtime_config_equal(
    a: &control_plane::KimiRuntimeConfig,
    b: &control_plane::KimiRuntimeConfig,
) -> bool {
    a == b
}

pub(crate) fn normalize_diff_file_path(path: &str) -> Option<String> {
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
pub(crate) fn parse_diff_file_paths(unified_diff: &str) -> Vec<String> {
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

pub(crate) fn diff_line_text(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

pub(crate) fn is_unified_file_boundary(lines: &[&str], idx: usize) -> bool {
    let line = diff_line_text(lines[idx]);
    line.starts_with("diff --git ")
        || (line.starts_with("--- ")
            && lines
                .get(idx + 1)
                .is_some_and(|next| diff_line_text(next).starts_with("+++ ")))
}

pub(crate) fn split_unified_diff_by_file(unified_diff: &str) -> Vec<(String, String)> {
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

pub(crate) fn external_diff_log_body(message: &str) -> Option<&str> {
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

pub(crate) fn parse_session_diff_file_paths(log_dir: &Path) -> Vec<String> {
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

pub(crate) fn resolve_diff_file_path(project_root: &Path, display_path: &str) -> Option<PathBuf> {
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

pub(crate) fn read_diff_file_text(
    project_root: &Path,
    display_path: &str,
) -> Option<Option<String>> {
    let path = resolve_diff_file_path(project_root, display_path)?;
    match std::fs::read_to_string(path) {
        Ok(text) => Some(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    }
}

pub(crate) struct ExternalDiffDelta {
    pub(crate) files_changed: Vec<String>,
    pub(crate) unified_diff: String,
}

#[derive(Default)]
pub(crate) struct ExternalDiffDeltaTracker {
    snapshots: HashMap<String, Option<String>>,
}

impl ExternalDiffDeltaTracker {
    pub(crate) fn seed_current_paths<'a>(
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

    pub(crate) fn seed_from_session_log(&mut self, project_root: &Path, log_dir: &Path) {
        let paths = parse_session_diff_file_paths(log_dir);
        self.seed_current_paths(project_root, paths.iter().map(String::as_str));
    }

    pub(crate) fn delta(
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
pub(crate) async fn resolve_agent_backend(
    shared: &Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>>,
    project: &Project,
) -> Option<external_agent::AgentBackend> {
    resolve_agent_backend_from_config(shared.read().await.clone(), project)
}

pub(crate) fn codex_context_trace_dir(
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

fn clear_consumed_kimi_fork_staging(config: &mut crate::session_config::SessionAgentConfig) {
    // These fields authorize one exact head-check + rollback while creating
    // an anchor child. They are not durable session profile: once start_thread
    // succeeds, retaining them in either the child wrapper or the parent
    // overlay makes an ordinary later resume try to fork again.
    config.kimi_fork_rollback_turns = None;
    config.kimi_fork_expected_horizon = None;
}

/// Construct, initialize, and start a thread for an external agent backend.
///
/// Returns the agent, thread handle, and event receiver. The caller owns the
/// agent lifetime and is responsible for sending messages and draining events.
#[allow(clippy::too_many_arguments)] // established internal signature: the params are distinct dependencies, not a bundle
pub(crate) async fn create_external_agent(
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

    // Select and hold a leased OAuth home before any backend constructor can
    // spawn. Expiry/revocation during initialize/start_thread parks cleanup;
    // success atomically promotes this provisional nonce to the real wrapper
    // and backend identities below, while every `?` error drops the guard and
    // re-sweeps.
    let leased_home_startup =
        crate::credential_leases::hold_leased_home_for_external_startup(backend.as_short_str())
            .map_err(|error| {
                CallerError::ExternalAgent(format!(
                    "prepare leased credential home for {backend}: {error}"
                ))
            })?;
    let mcp_session_id = mcp_session_id.or_else(|| session_log_id(session_log));
    let mcp_auth_token =
        web_port.map(|_| crate::web_gateway::loopback_mcp_auth_token().to_string());
    // Daemon-side MCP status ground truth: the gate reports this session's
    // first `/mcp` serves (initialize, tools/list) into its timeline the
    // moment they happen — the backend's own MCP status echo only arrives
    // at turn boundaries. Registered before the spawn below so the
    // client's very first request is attributable; a respawn re-registers
    // and reports afresh.
    if mcp_auth_token.is_some() {
        if let Some(session_id) = mcp_session_id.as_deref() {
            crate::web_gateway::register_supervised_mcp_session(session_id, session_log);
        }
    }
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

    // Anchor-fork staging (codex): one-shot spawn parameters the fork
    // orchestrator persisted into the wrapper's launch config. Lifted only
    // while the wrapper still resumes the parent id (the same window as
    // `fork_resume`), so a later plain resume of the child can never
    // re-fork; the announce-time overlay persist strips them durably.
    let (codex_fork_rollout_path, codex_fork_cut) = if fork_resume {
        session_log
            .lock()
            .ok()
            .map(|log| log.dir().to_path_buf())
            .and_then(|dir| crate::session_config::read_log_dir_config(&dir))
            .map(|cfg| {
                let cut = if let Some(item_id) = cfg
                    .codex_fork_rollback_item_id
                    .filter(|item| !item.trim().is_empty())
                {
                    Some(crate::session_fork::CodexForkCut::ItemAnchor {
                        item_id,
                        position: cfg
                            .codex_fork_rollback_position
                            .unwrap_or_else(|| "after".to_string()),
                    })
                } else {
                    cfg.codex_fork_rollback_turns
                        .map(crate::session_fork::CodexForkCut::Turns)
                };
                (
                    cfg.codex_fork_rollout_path.map(std::path::PathBuf::from),
                    cut,
                )
            })
            .unwrap_or((None, None))
    } else {
        (None, None)
    };

    let dns_credential_store = Some(crate::access::backend::select_backend().cert_dir());
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
            let effective_command = cfg.effective_command(codex_managed_context);
            let protocol_watch = external_agent::protocol_watch::ProtocolWatchHandle::new_in(
                crate::platform::intendant_home(),
                AgentBackend::Codex,
                if codex_managed_context {
                    "managed"
                } else {
                    "vanilla"
                },
                &effective_command,
            );
            // Managed sessions spawn the Intendant-aware fork when one is
            // configured (`codex.managed_command`); vanilla sessions and
            // legacy configs use `codex.command`.
            let agent = Box::new(external_agent::codex::CodexAgent::with_options(
                effective_command,
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
                dns_credential_env: crate::credential_leases::configured_dns_credential_child_scrub(
                ),
                dns_credential_store: dns_credential_store.clone(),
                mcp_session_id: mcp_session_id.clone(),
                resume_session: resume_session.clone(),
                fork_resume,
                fork_from_rollout_path: codex_fork_rollout_path.clone(),
                fork_cut: codex_fork_cut.clone(),
                kimi_fork_rollback_turns: None,
                kimi_fork_expected_horizon: None,
                kimi_allowed_tools: None,
                codex_home,
                protocol_watch,
            };
            (agent, config)
        }
        AgentBackend::ClaudeCode => {
            let cfg = &project.config.agent.claude_code;
            let protocol_watch = external_agent::protocol_watch::ProtocolWatchHandle::new_in(
                crate::platform::intendant_home(),
                AgentBackend::ClaudeCode,
                "default",
                &cfg.command,
            );
            let agent = Box::new(
                external_agent::claude_code::ClaudeCodeAgent::new(
                    cfg.command.clone(),
                    cfg.model.clone(),
                    cfg.permission_mode.clone(),
                    cfg.effort.clone(),
                    cfg.allowed_tools.clone(),
                    web_port,
                )
                .with_max_budget_usd(cfg.max_budget_usd),
            );
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
                dns_credential_env: crate::credential_leases::configured_dns_credential_child_scrub(
                ),
                dns_credential_store,
                mcp_session_id: mcp_session_id.clone(),
                resume_session: resume_session.clone(),
                fork_resume,
                fork_from_rollout_path: None,
                fork_cut: None,
                kimi_fork_rollback_turns: None,
                kimi_fork_expected_horizon: None,
                kimi_allowed_tools: None,
                codex_home: None,
                protocol_watch,
            };
            (agent, config)
        }
        AgentBackend::Kimi => {
            let cfg = &project.config.agent.kimi;
            let protocol_watch = external_agent::protocol_watch::ProtocolWatchHandle::new_in(
                crate::platform::intendant_home(),
                AgentBackend::Kimi,
                "server-v1",
                &cfg.command,
            );
            let launch = external_agent::kimi_code::KimiLaunchConfig {
                model: cfg.model.clone(),
                thinking: project::normalize_kimi_thinking(cfg.thinking.as_deref()),
                permission_mode: project::normalize_kimi_permission_mode(&cfg.permission_mode),
                allowed_tools: cfg.allowed_tools.clone(),
                plan_mode: cfg.plan_mode,
                swarm_mode: cfg.swarm_mode,
            };
            let agent = Box::new(external_agent::kimi_code::KimiCodeAgent::new(
                cfg.command.clone(),
                launch,
                web_port,
            ));
            let config = AgentConfig {
                model: cfg.model.clone(),
                working_dir: project.root.clone(),
                request_trace_dir: None,
                request_trace_temporary: false,
                context_archive: "off".to_string(),
                approval_policy: project::normalize_kimi_permission_mode(&cfg.permission_mode),
                sandbox: String::new(),
                reasoning_effort: project::normalize_kimi_thinking(cfg.thinking.as_deref()),
                service_tier: None,
                web_search: false,
                network_access: false,
                writable_roots: Vec::new(),
                codex_managed_context: false,
                web_port,
                mcp_auth_token: mcp_auth_token.clone(),
                dns_credential_env: crate::credential_leases::configured_dns_credential_child_scrub(
                ),
                dns_credential_store,
                mcp_session_id: mcp_session_id.clone(),
                resume_session: resume_session.clone(),
                fork_resume,
                fork_from_rollout_path: None,
                fork_cut: None,
                kimi_fork_rollback_turns: cfg.kimi_fork_rollback_turns,
                kimi_fork_expected_horizon: cfg.kimi_fork_expected_horizon.clone(),
                kimi_allowed_tools: cfg.allowed_tools.clone(),
                codex_home: None,
                protocol_watch,
            };
            (agent, config)
        }
        AgentBackend::Pi => {
            let cfg = &project.config.agent.pi;
            let thinking = project::normalize_pi_thinking(cfg.thinking.as_deref());
            let protocol_watch = external_agent::protocol_watch::ProtocolWatchHandle::new_in(
                crate::platform::intendant_home(),
                AgentBackend::Pi,
                "rpc",
                &cfg.command,
            );
            let launch = external_agent::pi::PiLaunchConfig {
                model: cfg.model.clone(),
                thinking: thinking.clone(),
                allowed_tools: cfg
                    .allowed_tools
                    .as_deref()
                    .map(project::normalize_pi_allowed_tools),
            };
            let agent = Box::new(external_agent::pi::PiAgent::new(
                cfg.command.clone(),
                launch,
                web_port,
            ));
            let config = AgentConfig {
                model: cfg.model.clone(),
                working_dir: project.root.clone(),
                request_trace_dir: None,
                request_trace_temporary: false,
                context_archive: "off".to_string(),
                approval_policy: "on-request".to_string(),
                sandbox: String::new(),
                reasoning_effort: thinking,
                service_tier: None,
                web_search: false,
                network_access: false,
                writable_roots: Vec::new(),
                codex_managed_context: false,
                web_port,
                mcp_auth_token: mcp_auth_token.clone(),
                dns_credential_env: crate::credential_leases::configured_dns_credential_child_scrub(
                ),
                dns_credential_store,
                mcp_session_id: mcp_session_id.clone(),
                resume_session: resume_session.clone(),
                fork_resume,
                fork_from_rollout_path: None,
                fork_cut: None,
                kimi_fork_rollback_turns: None,
                kimi_fork_expected_horizon: None,
                kimi_allowed_tools: None,
                codex_home: None,
                protocol_watch,
            };
            (agent, config)
        }
    };

    let event_rx = crate::credential_leases::scope_leased_home_for_external_startup(
        leased_home_startup.as_ref(),
        agent.initialize(config),
    )
    .await?;
    slog(session_log, |l| l.debug("External agent initialized"));

    // Kimi's native session history is written below a per-wrapper bridge,
    // not necessarily the user's default KIMI_CODE_HOME. Persist that exact
    // prepared root before start_thread can create or mutate native history:
    // if the daemon exits during the first attach/create RPC, the next
    // catalog/search/fork-point reader can still recover the bridge. The
    // config writer is atomic; failure is fail-closed so we never knowingly
    // create history that Intendant cannot rediscover.
    if *backend == AgentBackend::Kimi {
        let persistence = (|| {
            let mut launch = agent.launch_config_snapshot().ok_or_else(|| {
                "Kimi adapter did not expose its prepared launch config".to_string()
            })?;
            if launch.kimi_home.is_none() {
                return Err("Kimi adapter did not expose its prepared bridge home".to_string());
            }
            let log_dir = session_log
                .lock()
                .map_err(|_| "session log lock poisoned".to_string())?
                .dir()
                .to_path_buf();
            if let Some(existing) = crate::session_config::read_log_dir_config(&log_dir) {
                launch.merge_missing_from(existing);
            }
            crate::session_config::write_log_dir_config(&log_dir, &launch)?;
            if !fork_resume {
                if let Some(resume) = resume_session
                    .as_deref()
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                {
                    crate::session_config::write_external_overlay(
                        &crate::platform::home_dir(),
                        "kimi",
                        resume,
                        &launch,
                    )?;
                }
            }
            Ok::<(), String>(())
        })();
        if let Err(error) = persistence {
            let _ = agent.shutdown().await;
            return Err(CallerError::ExternalAgent(format!(
                "persist prepared Kimi bridge home: {error}"
            )));
        }
    }

    let thread = agent.start_thread().await?;
    if *backend == AgentBackend::Kimi {
        let persistence = (|| {
            let mut launch = agent.launch_config_snapshot().ok_or_else(|| {
                "Kimi adapter did not expose its consumed launch config".to_string()
            })?;
            let log_dir = session_log
                .lock()
                .map_err(|_| "session log lock poisoned".to_string())?
                .dir()
                .to_path_buf();
            if let Some(existing) = crate::session_config::read_log_dir_config(&log_dir) {
                launch.merge_missing_from(existing);
            }
            clear_consumed_kimi_fork_staging(&mut launch);
            crate::session_config::write_log_dir_config(&log_dir, &launch)
        })();
        if let Err(error) = persistence {
            let _ = agent.shutdown().await;
            return Err(CallerError::ExternalAgent(format!(
                "persist consumed Kimi launch config: {error}"
            )));
        }
    }
    if let Some(mut startup) = leased_home_startup {
        let canonical_backend_id = if backend.thread_id_is_canonical(&thread.thread_id) {
            thread.thread_id.as_str()
        } else {
            ""
        };
        if let Err(error) = startup.promote(&[
            mcp_session_id.as_deref().unwrap_or_default(),
            canonical_backend_id,
        ]) {
            // Revocation deliberately refuses promotion. Stop the process
            // while the provisional hold still pins its home, then Drop the
            // hold so queued cleanup cannot race a final credential refresh.
            let _ = agent.shutdown().await;
            drop(startup);
            return Err(CallerError::ExternalAgent(format!(
                "register lease-backed external session: {error}"
            )));
        }
    }
    slog(session_log, |l| {
        l.debug(&format!("External agent thread: {}", thread.thread_id))
    });

    Ok((agent, thread, event_rx))
}

/// Configuration for `drain_external_agent_events`.
pub(crate) struct DrainConfig<'a> {
    pub(crate) bus: &'a EventBus,
    pub(crate) session_id: Option<String>,
    pub(crate) alias_session_id: Option<String>,
    /// The backend (Codex) thread id of THIS conversation, when the caller
    /// holds the live `AgentThread`. Conversations are named inconsistently
    /// across paths — the CLI external-agent loop uses `session_id` = thread
    /// id with the Intendant session id as the alias, while the daemon's
    /// persistent dispatch loop uses `session_id` = Intendant session id with
    /// the thread id as the alias — so a thread action that targets this
    /// conversation by either name resolves its `threadId` from this field
    /// rather than guessing which of the two ids the backend understands.
    pub(crate) backend_thread_id: Option<String>,
    pub(crate) autonomy: SharedAutonomy,
    pub(crate) session_log: &'a SharedSessionLog,
    pub(crate) project_root: &'a Path,
    pub(crate) log_dir: &'a Path,
    pub(crate) approval_registry: &'a event::ApprovalRegistry,
    pub(crate) json_approval: Option<&'a JsonApprovalSlot>,
    /// Web dashboard port when serving (`--web`). `Some` means an interactive
    /// frontend exists, so external-agent approval requests are surfaced to
    /// the gate rather than auto-denied as if truly headless.
    pub(crate) web_port: Option<u16>,
    pub(crate) agent_source: Option<String>,
    /// When true, `ToolStarted` just increments the turn counter without
    /// emitting `AgentStarted`. Legacy presence paths set this to avoid
    /// duplicating model activity already shown via `ModelResponse`. Kimi's
    /// precise server-v1 tool ids make its daemon lane safe to leave
    /// unsuppressed, preserving correlated tool start/output telemetry.
    pub(crate) suppress_agent_started: bool,
    /// When set (supervised sessions with their own session log), the drain
    /// persists model responses and reasoning inline into the owning
    /// session's log (`persist_external_model_response_*_if_needed`) and its
    /// `ModelResponse` bus events skip the session-log writer lane
    /// ([`DrainConfig::send_model_response`]) — each response persists
    /// exactly once, in the owning log, and the daemon head log does not
    /// aggregate a second copy. When unset (foreground shapes sharing the
    /// writer's log), the bus writer is the response's only path to disk.
    pub(crate) persist_model_responses_inline: bool,
    /// When true and no `json_approval` slot is set, auto-deny approval
    /// requests (headless mode with no interactive input).
    pub(crate) headless: bool,
    /// Shared context-injection queue. Fallback target when the backend
    /// does not support mid-turn steering — queued items are drained on
    /// the next turn's follow-up message path.
    pub(crate) context_injection: &'a event::ContextInjectionQueue,
    /// Reload-credentials handshake with the supervision loop: when a
    /// `ReloadBackendCredentials` event arrives mid-turn, the drain
    /// interrupts the backend (stop semantics) and raises this flag; the
    /// loop applies the in-place respawn once the drain returns. `None`
    /// for lanes without the in-loop respawn (the foreground persistent
    /// loop), where the event is simply not consumable mid-turn.
    pub(crate) reload_credentials: Option<&'a std::sync::atomic::AtomicBool>,
    /// The supervised session's coordination-bus declaration, owned by
    /// the supervision loop (Track C §1.5: the supervisor writes for the
    /// backend). The drain's event ticks are the wrapper's heartbeat
    /// boundary — internally throttled, so a busy drain costs one mtime
    /// touch a minute. `None` for lanes without a session declaration
    /// (child/side drains inherit the parent's guard; the legacy
    /// persistent presence lane and tests pass `None`).
    pub(crate) coordination_declaration:
        Option<&'a crate::coordination::lifecycle::SessionDeclarationGuard>,
}

impl DrainConfig<'_> {
    /// Emit a `ModelResponse` bus event with the persistence disposition
    /// this drain already applied. When `persist_model_responses_inline` is
    /// set, the drain wrote the response (and any reasoning) into the owning
    /// session's log before emitting, so the bus copy must skip the
    /// session-log writer lane; otherwise the writer is the event's only
    /// path to disk and the full send is load-bearing.
    pub(crate) fn send_model_response(&self, event: AppEvent) {
        if self.persist_model_responses_inline {
            self.bus.send_already_persisted(event);
        } else {
            self.bus.send(event);
        }
    }
}

pub(crate) const EXTERNAL_CONTEXT_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(1);

/// How often the turn drain consults the radar for §2.8 ALERT steers —
/// the radar itself republishes on `coordination::radar::RADAR_TICK`,
/// so a faster consult would only re-read identical snapshots.
pub(crate) const EXTERNAL_COORDINATION_CONSULT_INTERVAL: Duration = Duration::from_secs(5);

/// The external ALERT-only delivery lane's gate (Track C §2.8, R8):
/// consult the daemon-published radar state for this session's alerts,
/// render the schema-rendered single-line steers for its distinct
/// overlap sets ([`coordination::render::render_alert_steers`] — ALERT
/// overlaps only; ambient content has no path in), and admit them
/// through the daemon-side cooldown ledger (one steer per set per pair,
/// 10-minute per-session cooldown; suppressed sets retry on a later
/// consult). Admitted steers are RECORDED — the caller must deliver
/// each: mid-turn through the backend's `steer_turn` lane, otherwise as
/// the targeted-`ContextInjection` between-turns fallback
/// ([`queue_external_coordination_alert_steers`], merged into the next
/// outgoing prompt by `drain_steer_queue_as_followup`). Returns nothing
/// outside a radar-publishing daemon (foreground `--agent` shapes) —
/// the ruled degrade-to-no-op.
pub(crate) fn admit_external_coordination_alert_steers(
    session_id: Option<&str>,
) -> Vec<coordination::render::AlertSteer> {
    let Some(session_id) = session_id else {
        return Vec::new();
    };
    let Some(state) = coordination::radar::published_radar_state() else {
        return Vec::new();
    };
    let writer_id = coordination::lifecycle::writer_id_for_session(session_id);
    let Some(snapshot) = state.space_with_alerts_for(&writer_id) else {
        return Vec::new();
    };
    let steers = coordination::render::render_alert_steers(&snapshot, &writer_id);
    if steers.is_empty() {
        return steers;
    }
    let now_ms = coordination::now_ms();
    let ledger = coordination::radar::external_steer_ledger();
    steers
        .into_iter()
        .filter(|steer| ledger.admit(session_id, steer.set_hash, now_ms))
        .collect()
}

/// Between-turns half of the §2.8 lane: queue admitted steers as
/// session-TARGETED system context injections; the external follow-up
/// path (`drain_steer_queue_as_followup`) merges them verbatim above
/// the next outgoing prompt. Deliberately NO steer lifecycle events —
/// a supervisor-originated radar line is not a user steer (the
/// managed-context density steer's altitude); the session log carries
/// the delivery fact instead. Returns how many lines were queued.
pub(crate) fn queue_external_coordination_alert_steers(
    context_injection: &event::ContextInjectionQueue,
    session_id: Option<&str>,
    session_log: &SharedSessionLog,
) -> usize {
    let steers = admit_external_coordination_alert_steers(session_id);
    if steers.is_empty() {
        return 0;
    }
    let queued = steers.len();
    if let Ok(mut queue) = context_injection.lock() {
        for steer in steers {
            slog(session_log, |l| {
                l.info(&format!(
                    "Coordination radar ALERT queued for the next turn: {}",
                    steer.text
                ))
            });
            queue.push(event::ContextInjection {
                text: steer.text,
                images: Vec::new(),
                source: event::InjectionSource::System,
                target_session_id: session_id.map(str::to_string),
                steer_id: None,
            });
        }
    }
    queued
}

/// Mid-turn half of the §2.8 lane, called from the turn drain on its
/// consult cadence: admitted steers go through the backend's native
/// `steer_turn`; a backend that cannot take one right now (turn just
/// ended, RPC trouble) falls back to the queued between-turns shape so
/// the line still reaches the model exactly once.
pub(crate) async fn steer_external_coordination_alerts(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    config: &DrainConfig<'_>,
) {
    let steers = admit_external_coordination_alert_steers(config.session_id.as_deref());
    for steer in steers {
        match agent.steer_turn(&steer.text).await {
            Ok(()) => {
                let content = format!(
                    "Coordination radar ALERT steered into the running {} turn: {}",
                    agent.name(),
                    steer.text
                );
                slog(config.session_log, |l| l.info(&content));
                config.bus.send(AppEvent::LogEntry {
                    session_id: config.session_id.clone(),
                    level: "info".to_string(),
                    source: "Intendant".to_string(),
                    content,
                    turn: None,
                });
            }
            Err(e) => {
                slog(config.session_log, |l| {
                    l.debug(&format!(
                        "Coordination radar steer fell back to the next-turn queue ({e})"
                    ))
                });
                if let Ok(mut queue) = config.context_injection.lock() {
                    queue.push(event::ContextInjection {
                        text: steer.text,
                        images: Vec::new(),
                        source: event::InjectionSource::System,
                        target_session_id: config.session_id.clone(),
                        steer_id: None,
                    });
                }
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct ExternalContextSnapshotState {
    pub(crate) emitted_keys: std::collections::HashSet<u64>,
    pub(crate) last_error: Option<String>,
}

/// Result of draining one batch of external agent events.
pub(crate) enum DrainOutcome {
    /// The agent's turn completed. The caller decides how to continue
    /// (e.g., wait for follow-up, emit DoneSignal, break inner loop).
    TurnCompleted {
        message: Option<String>,
        turns_in_round: usize,
    },
    /// A fatal (non-retryable, non-recoverable) backend error ended the
    /// round before any turn completed — the launch-refusal class: an
    /// invalid `--model` pin, an auth refusal at spawn. The backend
    /// process may still be running, but the round did no work and the
    /// error is its honest outcome. The caller must end with a FAILED
    /// terminal ([`crate::event::TaskOutcome::Failed`]), never a
    /// DoneSignal: a scheduled occurrence resolved by this shape journals
    /// `failed` (suspend streaks, owner visibility), not `completed`
    /// (2026-07-26: a fable-5 launch refusal rode a DoneSignal into a
    /// COMPLETED occurrence and the failure was invisible — specimen
    /// occurrence 21fe746a, session 6993c73f).
    TurnFailed {
        reason: String,
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
        request: Box<ExternalContextRewindRequest>,
        message: Option<String>,
        turns_in_round: usize,
        turn_stop_status: ManagedContextRewindTurnStopStatus,
    },
    /// The turn ended rejected at a provider usage limit
    /// ([`external_agent::AgentEvent::TurnLimitRejected`]). The backend
    /// process stays usable; the caller must consume no round budget and
    /// must NOT immediately re-fire — instead it parks the pending
    /// follow-up until `resets_at_epoch` (plus jitter; exponential
    /// backoff when absent) and queues user input arriving meanwhile.
    ///
    /// `turn_had_started` is the park's delivery awareness: `false` is
    /// the classic instant-rejection shape (the round did no work; the
    /// driving message never reached the model, so the park re-sends it
    /// verbatim — true at-least-once). `true` means the drain observed
    /// primary-turn work (assistant output, tool activity, a completed
    /// turn) before the rejection: the backend consumed — and, for
    /// backends with persistent rollouts, durably recorded — the driving
    /// message, so re-sending it would put the goal in the conversation
    /// twice ([`limit_park_pending`] parks a resume nudge instead;
    /// observed live 2026-07-28, session 800e6f58: a five_hour rejection
    /// ~90s into the first turn re-sent the whole goal at reset, and the
    /// backend re-read its entire mandate).
    LimitRejected {
        resets_at_epoch: Option<u64>,
        message: Option<String>,
        turn_had_started: bool,
    },
    /// A fatal backend error whose cause classifies as a TEMPORARY
    /// service condition ([`transient_service_condition`]) ended the
    /// round — the provider-incident class the interruption family was
    /// missing (2026-07-29 specimens 24f01636/13e53300: API-500
    /// round-deaths rode a DoneSignal and stranded both commissions
    /// fake-idle for over an hour, invisible to the credential-reload
    /// lane and every wake clock). The backend process stays usable and
    /// its conversation holds whatever the round delivered; the caller
    /// must count no round and arm the service-condition error park
    /// ([`transient_round_death_error_park`]) — bounded widening wakes,
    /// visible suspension when the schedule exhausts — instead of
    /// completing or failing the round. `turn_had_started` is the same
    /// delivery awareness as [`DrainOutcome::LimitRejected`]'s: whether
    /// the primary thread demonstrably engaged this round's driving
    /// message. Permanent-cause deaths never take this shape — they keep
    /// their terminal outcomes ([`DrainOutcome::TurnFailed`] at zero
    /// turns, the completion shape after real work).
    TransientRoundDeath {
        reason: String,
        turns_in_round: usize,
        turn_had_started: bool,
    },
    /// A fatal backend error whose cause classifies as a PROVIDER
    /// SAFEGUARDS FLAG ([`safeguards_flag_condition`]) ended the round —
    /// terminal for these bytes. The flag is the provider's judgment
    /// about the conversation's content, so a retry, park, or resume of
    /// the same context re-flags forever (2026-07-31 specimens: session
    /// 69c8535e's flag rode a DoneSignal into a COMPLETED occurrence and
    /// died invisible; a resume into session 77c8beaf's flagged context
    /// re-flagged immediately, three times in one arc). The caller must
    /// end the session with a FAILED terminal carrying the full cause,
    /// stamp the durable flag on the session meta (the boot sweep and
    /// readopt read it — they list, never nudge, this class), raise the
    /// safeguards attention surfaces, and surface queued/injected input
    /// as undelivered with the named reason. NO auto-retry lane exists
    /// for this class, ever, and no model fallback anywhere — the remedy
    /// is the owner's alone: a fresh session with the task RECAST in
    /// their own words (a judgment act, not mechanics).
    SafeguardsFlagged {
        reason: String,
        turns_in_round: usize,
    },
}

// ---------------------------------------------------------------------------
// Rate-limit park policy (park-until-reset for limit-rejected turns)
// ---------------------------------------------------------------------------

/// Jitter added on top of the provider's reset time so a fleet of parked
/// sessions doesn't stampede the API the second a window opens.
pub(crate) const LIMIT_PARK_JITTER_MIN_SECS: u64 = 30;
pub(crate) const LIMIT_PARK_JITTER_MAX_SECS: u64 = 90;
/// Backoff bounds when the rejection carried no reset time.
const LIMIT_PARK_BACKOFF_MIN_SECS: u64 = 5 * 60;
const LIMIT_PARK_BACKOFF_MAX_SECS: u64 = 30 * 60;
/// Cap on a single park cycle. A `seven_day` window can honestly reset
/// days out; instead of one multi-day timer, the park re-checks at this
/// cadence (one cheap rejected request per cycle re-parks with a fresh
/// reset time).
const LIMIT_PARK_MAX_SECS: u64 = 6 * 3600;

/// Random park jitter in the fleet-safe band. Tests inject their own
/// value into [`limit_park_delay`] instead of calling this.
/// `INTENDANT_LIMIT_PARK_JITTER_SECS` overrides the draw (clamped to the
/// band's max) — the e2e suite pins the park's wake-and-resume arc with
/// it, and an operator can flatten the fleet-safety jitter deliberately;
/// an unparsable value keeps the random draw.
pub(crate) fn limit_park_jitter_secs() -> u64 {
    use rand::Rng;
    if let Some(forced) = std::env::var("INTENDANT_LIMIT_PARK_JITTER_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
    {
        return forced.min(LIMIT_PARK_JITTER_MAX_SECS);
    }
    rand::thread_rng().gen_range(LIMIT_PARK_JITTER_MIN_SECS..=LIMIT_PARK_JITTER_MAX_SECS)
}

/// How long a limit-rejected follow-up parks before it is re-sent. Pure —
/// clock and jitter injected. With a wire reset time: until the reset
/// plus jitter, capped at [`LIMIT_PARK_MAX_SECS`]. Without one:
/// exponential backoff by consecutive-park `streak` (1-based), 5 → 30
/// minutes, so an untimed limit is retried patiently instead of hammered.
pub(crate) fn limit_park_delay(
    resets_at_epoch: Option<u64>,
    now_epoch: u64,
    streak: u32,
    jitter_secs: u64,
) -> Duration {
    let secs = match resets_at_epoch {
        Some(resets_at) => resets_at
            .saturating_sub(now_epoch)
            .min(LIMIT_PARK_MAX_SECS)
            .saturating_add(jitter_secs),
        None => {
            let shift = streak.saturating_sub(1).min(3);
            (LIMIT_PARK_BACKOFF_MIN_SECS << shift).min(LIMIT_PARK_BACKOFF_MAX_SECS)
        }
    };
    Duration::from_secs(secs)
}

/// What a park is waiting out. Both kinds ride the SAME armed-park state,
/// wake timer, queue-while-parked, cancel, and reload-preserve machinery —
/// the kind only decides the wake clock's policy at arm time and the
/// honest wording of the shared lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParkKind {
    /// A provider usage limit rejected the turn; the wake clock is the
    /// limit's reset time (plus jitter; backoff when untimed).
    ProviderLimit,
    /// A temporary service condition (the provider-incident class:
    /// repeated 5xx after the backend's own transport retries, gateway
    /// drops, stream cuts) killed the round; the wake clock is the
    /// bounded widening [`ERROR_PARK_BACKOFF_SCHEDULE_SECS`] schedule.
    ServiceCondition,
}

impl ParkKind {
    /// The park's display noun for the shared log lines ("<noun>
    /// elapsed/cancelled/…"), so an error park never claims to be
    /// rate-limited (the display-lie class the 2026-07-29 diagnosis
    /// stacked three of).
    pub(crate) fn noun(&self) -> &'static str {
        match self {
            ParkKind::ProviderLimit => "Rate-limit park",
            ParkKind::ServiceCondition => "Service-recovery pause",
        }
    }

    /// The queued-while-parked row for a user message held during a park.
    pub(crate) fn queued_log(&self) -> &'static str {
        match self {
            ParkKind::ProviderLimit => LIMIT_PARK_QUEUED_MESSAGE_LOG,
            ParkKind::ServiceCondition => {
                "Message queued — delivers when the service-recovery pause elapses"
            }
        }
    }

    /// The follow-up status detail for a message queued behind the park.
    pub(crate) fn queued_status_detail(&self) -> &'static str {
        match self {
            ParkKind::ProviderLimit => "rate-limited; delivers when the limit resets",
            ParkKind::ServiceCondition => {
                "waiting out a service condition; delivers when the recovery pause elapses"
            }
        }
    }

    /// The turn-status name emitted while this park kind holds the lane
    /// (the dashboard's waiting chip) — the arm sites and the
    /// held-through-terminal re-emits share it, so a hold can restore
    /// the exact status the observed round's "running" claim overwrote.
    pub(crate) fn waiting_turn_status(&self) -> &'static str {
        match self {
            ParkKind::ProviderLimit => "waiting-rate-limit",
            ParkKind::ServiceCondition => "waiting-service-recovery",
        }
    }

    /// The turn-status detail line paired with
    /// [`Self::waiting_turn_status`].
    pub(crate) fn waiting_turn_detail(&self, agent_name: &str) -> String {
        match self {
            ParkKind::ProviderLimit => {
                format!("{agent_name} rate-limited; parked until the limit resets")
            }
            ParkKind::ServiceCondition => format!(
                "{agent_name} waiting out a temporary service condition; parked for recovery"
            ),
        }
    }
}

/// The honest log row when a terminal classification lands while a park
/// with owed work is armed and the park outranks it — the park-then-die
/// reconciliation (2026-08-01 specimen e883a2db: a five_hour rejection
/// armed the park at 22:34:30.897 and the dying backend's exit was
/// classified a fatal round failure 1.5s later, whose `break` destroyed
/// the in-memory wake while the durable meta kept advertising
/// `has_pending: true` — a silent forever-strand). The classification is
/// usually the same rejection's death rattle; holding residence keeps
/// the armed wake, which respawns a confirmed-dead backend before
/// delivering.
pub(crate) fn park_holds_through_terminal_line(
    kind: ParkKind,
    classification: &str,
    reason: &str,
) -> String {
    let reason = reason.trim();
    let cause = if reason.is_empty() {
        String::new()
    } else {
        format!(" ({reason})")
    };
    format!(
        "{} holds through a backend terminal while parked — {classification}{cause}; \
         the session stays resident and resumes at the park's wake",
        kind.noun()
    )
}

/// The honest log row when a terminal classification lands while a
/// deferred mid-turn credential reload is still pending and the reload
/// outranks it (the 2026-08-02 limit-exit race, specimen 2c6ea80d: the
/// reload's deferral was swallowed by the turn's exit and the session
/// parked until the OLD account's 05:20 reset while the fresh store sat
/// unread). The pending respawn IS the recovery — an auth-shaped round
/// death is exactly what the fresh credentials may cure — so the session
/// stays resident and the loop's safe point applies the reload instead
/// of exiting.
pub(crate) fn reload_outranks_terminal_line(classification: &str, reason: &str) -> String {
    let reason = reason.trim();
    let cause = if reason.is_empty() {
        String::new()
    } else {
        format!(" ({reason})")
    };
    format!(
        "Deferred credential reload outranks a backend terminal — {classification}{cause}; \
         the session stays resident and respawns on the fresh credential store"
    )
}

/// Re-arming a park over an already-armed one (a second rejection or
/// round death while parked — the dying backend's death-rattle class)
/// must not clobber the owed work: while parked nothing delivers user
/// input, so the previous park's pending is still the owed re-send and
/// strictly outranks a replacement arm's synthesized resume nudge. The
/// fresh wake clock always wins; only the pending is preserved. Returns
/// whether an owed pending carried over, so the caller can re-state the
/// arm line with the truthful pending-ness.
pub(crate) fn inherit_owed_pending(
    previous: Option<LimitParkState>,
    park: &mut LimitParkState,
) -> bool {
    match previous.and_then(|prev| prev.pending) {
        Some(owed) => {
            park.pending = Some(owed);
            true
        }
        None => false,
    }
}

/// One armed park in an external-session lane: the lane sleeps until
/// `resume_at`, then re-sends `pending` (if still uncancelled). User
/// messages arriving while parked queue behind it instead of burning
/// against the unavailable backend. `kind` says what the park waits out
/// (a provider limit's reset, or a temporary service condition's
/// recovery schedule) — one slot, one wake timer, honest wording.
pub(crate) struct LimitParkState {
    pub(crate) resume_at: tokio::time::Instant,
    pub(crate) pending: Option<FollowUpMessage>,
    pub(crate) kind: ParkKind,
}

/// A continuation for a turn the backend had already STARTED when it was
/// cut mid-flight. The turn's driving message is in the backend's
/// conversation (Claude Code persists it to the rollout at delivery), so
/// the safe continuation is a short nudge naming the cause — never a
/// re-send of the original message, which would double it and make the
/// backend re-read its whole mandate. `None` when the turn never started:
/// the caller keeps its lane's never-delivered behavior (the rate-limit
/// park re-sends the full rejected message; the credential-reload respawn
/// queues nothing). Both mid-turn lanes ride this one constructor — the
/// reload respawn's synthesized continuation
/// (`external_mode::RELOAD_MIDTURN_CONTINUATION_TEXT`) and the limit
/// park's resume nudge ([`LIMIT_MIDTURN_CONTINUATION_TEXT`]) — one seam
/// for the delivery-aware decision, never two copies.
pub(crate) fn midturn_continuation(text: &str, turn_had_started: bool) -> Option<FollowUpMessage> {
    turn_had_started.then(|| FollowUpMessage::text(text.to_string()))
}

/// The resume nudge a rate-limit park re-sends when the provider limit
/// rejected a turn the backend had already started (see
/// [`limit_park_pending`]).
pub(crate) const LIMIT_MIDTURN_CONTINUATION_TEXT: &str =
    "A provider rate limit interrupted the previous turn mid-stream; the limit has reset — \
     continue where you left off. Do not expect the interrupted message to be re-sent: it is \
     already in this conversation.";

/// The message a rate-limit park re-sends at reset — delivery-aware, and
/// the one seam both park lanes (the supervised external-mode loop and
/// the persistent daemon lane) construct their pending from. When the
/// rejected round never started at the backend, the park pends `rejected`
/// itself (the merged text with its original attachments — the message
/// truly was never delivered). When the round HAD started, the park pends
/// a [`midturn_continuation`] resume nudge instead, inheriting the
/// rejected message's follow-up/steer ids so a user cancel during the
/// park cancels the resume exactly like it cancels a full re-send; the
/// nudge deliberately carries none of the rejected message's attachments
/// or edit/rewind directives — those were already applied when the turn
/// started, and re-playing an edit rollback would rewind the backend a
/// second time.
pub(crate) fn limit_park_pending(
    rejected: FollowUpMessage,
    turn_had_started: bool,
) -> FollowUpMessage {
    delivery_aware_park_pending(rejected, turn_had_started, LIMIT_MIDTURN_CONTINUATION_TEXT)
}

/// The one delivery-aware pending decision every park lane rides
/// (never copies): a round whose driving message never reached the
/// backend parks the message itself for a verbatim re-send (true
/// at-least-once); a round the backend had started parks a resume nudge
/// carrying the interruption's cause instead — inheriting the driving
/// message's follow-up/steer ids so a user cancel during the park
/// cancels the resume exactly like it cancels a full re-send, and
/// deliberately carrying none of its attachments or edit/rewind
/// directives (those were already applied when the turn started).
/// `midturn_text` names the cause: the rate-limit park passes
/// [`LIMIT_MIDTURN_CONTINUATION_TEXT`], the service-condition error park
/// [`ERROR_MIDTURN_CONTINUATION_TEXT`].
pub(crate) fn delivery_aware_park_pending(
    rejected: FollowUpMessage,
    turn_had_started: bool,
    midturn_text: &str,
) -> FollowUpMessage {
    match midturn_continuation(midturn_text, turn_had_started) {
        Some(mut nudge) => {
            nudge.follow_up_id = rejected.follow_up_id;
            nudge.steer_id = rejected.steer_id;
            nudge
        }
        None => rejected,
    }
}

/// A backend respawn class killed this session's still-running background
/// tasks (they were OS children of the replaced backend process, and the
/// replacement does not inherit them). Flip their registry records to
/// died-with-restart under the named cause, stamp the durable bg-park
/// marker's died form, log the honest line, and publish the attention
/// activity snapshot that replaces the stale `parked-on-tasks` claim —
/// the park marker is session-state and survives every respawn class
/// (credential reload, service-recovery restart, rate-limit restart,
/// daemon restart) while the tasks never do, and until this seam nothing
/// reconciled the two: a forever-park waiting on a dead wake.
///
/// Returns the died tasks' descriptions so a respawn lane that ALREADY
/// sends a delivery-aware continuation can carry the re-run offer on it
/// ([`died_tasks_nudge_addendum`]). NOTHING here re-executes a command or
/// re-arms delivery: commands are not known idempotent, so re-running is
/// an owner decision — the session card's one-tap re-run, or the model
/// deciding after an owner-visible nudge.
///
/// The same confirmed death also reconciles the session's pending native
/// scheduled wakeup ([`take_over_native_wakeup_at_respawn`]) — the one
/// wake source that IS re-armed across the respawn, because delivering a
/// recorded wake prompt executes nothing.
pub(crate) fn mark_parked_tasks_died_with_restart(
    bus: &EventBus,
    session_log: &SharedSessionLog,
    live_session_id: &Option<String>,
    backend_session_id: Option<&str>,
    cause: &str,
) -> Vec<String> {
    let Some(backend_session_id) = backend_session_id.map(str::trim).filter(|s| !s.is_empty())
    else {
        return Vec::new();
    };
    let now_epoch = crate::session_activity::epoch_seconds();
    // The same confirmed death also killed the harness's own ScheduleWakeup
    // timer — reconcile it BEFORE the task early-return: the wakeup class's
    // specimen was parked on nothing but its timer (zero background tasks).
    take_over_native_wakeup_at_respawn(
        bus,
        session_log,
        live_session_id,
        backend_session_id,
        cause,
        now_epoch,
    );
    let died = crate::background_tasks::mark_running_died_with_restart(
        backend_session_id,
        cause,
        now_epoch,
    );
    if died.is_empty() {
        return Vec::new();
    }
    let descs: Vec<String> = died
        .iter()
        .map(|record| record.description.clone())
        .collect();
    let line = format!(
        "⚠ {} background task{} died with {cause} — nothing is re-run automatically: {}",
        descs.len(),
        if descs.len() == 1 { "" } else { "s" },
        descs.join("; "),
    );
    slog(session_log, |l| l.warn(&line));
    bus.send(AppEvent::LogEntry {
        session_id: live_session_id.clone(),
        level: "warn".to_string(),
        source: "Intendant".to_string(),
        content: line,
        turn: None,
    });
    // Durable half: the boot pass and the drain wait set read this
    // marker, so a died park stays adjudicable after the daemon itself
    // restarts. Cleared when the session works again (the live-park
    // stamping seam observes any non-parked activity).
    slog(session_log, |l| {
        l.set_bg_park(Some(session_log::SessionBgParkMeta {
            tasks: descs.clone(),
            died_cause: Some(cause.to_string()),
            died_at_epoch: Some(now_epoch),
        }))
    });
    // Live half: replace the stale parked claim on every surface that
    // mirrors the vitals hub. The next publish from a live activity
    // machine (a resumed turn) clears the attention state.
    bus.send(AppEvent::SessionActivity {
        session_id: live_session_id.clone(),
        activity: died_tasks_attention_activity(descs.clone(), cause, now_epoch),
    });
    descs
}

/// The respawn-seam half of the native-wakeup takeover
/// ([`crate::native_wakeup`]): the confirmed backend death that killed
/// the parked background tasks above also killed the harness's own
/// `ScheduleWakeup` timer, so a still-pending record flips wrapper-owned
/// here under the named cause — the supervising loop's idle deadline arm
/// then delivers the wake at its due time. Announces the re-arm and
/// stamps the durable marker's re-armed form. Unlike background tasks —
/// commands, never re-run automatically — re-arming a timer is safe:
/// delivering the recorded wake prompt executes nothing. Idempotent: the
/// first seam to flip announces; later seams find the record already
/// wrapper-owned and say nothing.
pub(crate) fn take_over_native_wakeup_at_respawn(
    bus: &EventBus,
    session_log: &SharedSessionLog,
    live_session_id: &Option<String>,
    backend_session_id: &str,
    cause: &str,
    now_epoch: u64,
) -> Option<crate::native_wakeup::NativeWakeupRecord> {
    let taken = crate::native_wakeup::take_over_at_respawn(backend_session_id, cause)?;
    let line = format!(
        "⏰ The backend's native scheduled wakeup ({}) survived {cause}: Intendant re-armed \
         it and delivers the wake at its due time",
        crate::native_wakeup::due_phrase(taken.fire_at_epoch, now_epoch),
    );
    slog(session_log, |l| l.info(&line));
    bus.send(AppEvent::LogEntry {
        session_id: live_session_id.clone(),
        level: "info".to_string(),
        source: "Intendant".to_string(),
        content: line,
        turn: None,
    });
    slog(session_log, |l| l.set_native_wakeup(Some(taken.to_meta())));
    Some(taken)
}

/// The lost-timer note a delivery lane appends to a continuation it
/// ALREADY sends (the #644 composition law — never a minted message):
/// the session's native scheduled wakeup died with no re-arm possible
/// (`died_cause` set — daemon restart, session end), so the model must
/// re-arm its own cadence if it still wants one. Carries the original
/// wake prompt (bounded at record time) so nothing is silently lost.
pub(crate) fn died_wakeup_nudge_addendum(
    meta: &crate::session_log::SessionNativeWakeupMeta,
    now_epoch: u64,
) -> Option<String> {
    let cause = meta.died_cause.as_deref()?;
    let reason = meta
        .reason
        .as_deref()
        .map(|r| format!(" (reason: {r})"))
        .unwrap_or_default();
    let prompt = if meta.prompt.trim().is_empty() {
        "(empty)".to_string()
    } else {
        meta.prompt.clone()
    };
    Some(format!(
        " Note: your native scheduled wakeup ({}){reason} died with {cause} and was NOT \
         re-armed — re-arm it if you still need the cadence. Its wake prompt was: {prompt}",
        crate::native_wakeup::due_phrase(meta.fire_at_epoch, now_epoch),
    ))
}

/// The honest between-turn snapshot after a respawn killed the parked
/// tasks: no turn running, no live tasks — died ones, with the named
/// cause. `Idle` is the truthful state (nothing is happening); the died
/// fields are what make it an attention state on the dashboard.
pub(crate) fn died_tasks_attention_activity(
    died_background_tasks: Vec<String>,
    cause: &str,
    now_epoch: u64,
) -> crate::types::SessionActivityVitals {
    crate::types::SessionActivityVitals {
        state: crate::types::SessionActivityState::Idle,
        since_epoch: now_epoch,
        last_stream_byte_epoch: now_epoch,
        stalled_after_seconds: None,
        resets_at_epoch: None,
        background_tasks: Vec::new(),
        died_background_tasks,
        died_tasks_cause: Some(cause.to_string()),
    }
}

/// The re-run OFFER a respawn lane appends to a continuation nudge it
/// ALREADY sends (#644's delivery-aware machinery — never a second lane):
/// `None` when no tasks died, and never a message of its own — a respawn
/// that owes the model no continuation keeps owing none (the attention
/// state and the session card's one-tap re-run are the surfaces then),
/// and a verbatim re-send of the owner's message is never mutated. Text
/// only; the daemon never re-executes the command itself.
pub(crate) fn died_tasks_nudge_addendum(descs: &[String], cause: &str) -> Option<String> {
    match descs {
        [] => None,
        [desc] => Some(format!(
            " Note: the background task you were waiting on (\"{desc}\") died with {cause} and \
             was NOT re-run automatically (commands are not known idempotent) — re-run it \
             yourself if the work still needs it."
        )),
        many => Some(format!(
            " Note: {} background tasks you were waiting on died with {cause} and were NOT \
             re-run automatically (commands are not known idempotent): {}. Re-run them yourself \
             if the work still needs them.",
            many.len(),
            many.join("; "),
        )),
    }
}

/// Keep the durable bg-park marker (`SessionMeta::bg_park`) mirroring
/// the backend's own activity claims — the write side of the marker the
/// boot pass and the drain wait set adjudicate from. A parked-on-tasks
/// claim stamps the live form; any turn state clears the marker (the
/// session demonstrably works again — this is also what resolves a
/// died-with-restart attention state); a quiet idle clears only a LIVE
/// marker, never a died one (a respawned backend settling through an
/// empty round must not erase the statement that its predecessor's
/// tasks died).
pub(crate) fn stamp_bg_park_marker_from_activity(
    session_log: &SharedSessionLog,
    activity: &crate::types::SessionActivityVitals,
) {
    use crate::types::SessionActivityState as S;
    match activity.state {
        S::ParkedOnTasks => slog(session_log, |l| {
            l.set_bg_park(Some(session_log::SessionBgParkMeta {
                tasks: activity.background_tasks.clone(),
                died_cause: None,
                died_at_epoch: None,
            }))
        }),
        S::Idle | S::Stalled => slog(session_log, |l| l.clear_bg_park_if_live()),
        S::AwaitingApi | S::Reasoning | S::Responding | S::ToolRunning | S::RateLimited => {
            slog(session_log, |l| l.set_bg_park(None))
        }
    }
}

/// The named cause stamped on background tasks a service-condition
/// round death took with the backend process (the error park's respawn
/// class — the replacement process spawns at the park's wake).
pub(crate) const SERVICE_RECOVERY_RESTART_CAUSE: &str = "the service-recovery restart";

/// The named cause stamped on background tasks that died with a
/// limit-killed backend process (the rate-limit park's respawn class).
pub(crate) const RATE_LIMIT_RESTART_CAUSE: &str = "the rate-limit restart";

/// The named cause stamped on background tasks that died with the whole
/// daemon (the boot pass's respawn class — every backend process was a
/// child of the dead boot).
pub(crate) const DAEMON_RESTART_CAUSE: &str = "the daemon restart";

/// Park-arm composition of [`mark_parked_tasks_died_with_restart`]: a
/// round death may or may not have taken the backend process with it —
/// an API-500 against a live process leaves its background children
/// running and the park honest, so marking is gated on the backend's own
/// confirmed-exit probe (`next_round_reads_fresh_credentials`: true
/// exactly when no live backend process remains; backends without the
/// probe never mark here). Returns the re-run addendum for the park's
/// pending exactly when that pending is the synthesized resume nudge
/// (`turn_had_started`) — a verbatim re-send of the owner's own message
/// is never mutated, and no nudge is ever minted for this.
pub(crate) fn mark_died_tasks_at_park_arm(
    agent: &mut Box<dyn external_agent::ExternalAgent>,
    bus: &EventBus,
    session_log: &SharedSessionLog,
    live_session_id: &Option<String>,
    backend_session_id: Option<&str>,
    cause: &str,
    turn_had_started: bool,
) -> Option<String> {
    if !agent.next_round_reads_fresh_credentials() {
        return None;
    }
    let descs = mark_parked_tasks_died_with_restart(
        bus,
        session_log,
        live_session_id,
        backend_session_id,
        cause,
    );
    if !turn_had_started {
        return None;
    }
    died_tasks_nudge_addendum(&descs, cause)
}

/// The session-log/activity row announcing a park. One place so the two
/// lanes (persistent daemon lane and the supervised external-mode lane)
/// cannot drift. `has_pending` says whether a rejected message will be
/// re-sent when the park elapses.
pub(crate) fn limit_park_log_line(
    resets_at_epoch: Option<u64>,
    now_epoch: u64,
    has_pending: bool,
) -> String {
    let tail = if has_pending {
        "will auto-resume and re-send the pending message (messages arriving meanwhile queue)"
    } else {
        "messages arriving meanwhile queue until the limit resets"
    };
    format!(
        "Rate-limited — parked; {}; {tail}",
        external_agent::limit_reset_phrase(resets_at_epoch, now_epoch)
    )
}

/// The park a BACKEND-STARTED round arms when a provider limit rejects
/// it — the observed-from-idle lane in the supervised external-mode loop
/// and the spontaneous-round lane in the persistent daemon lane. No
/// driving message from this side exists to re-send (the backend woke
/// itself: its own background task, a native timer), so the pending is
/// the delivery-aware resume nudge when the turn had started and nothing
/// when the rejection arrived before any work; either way the armed
/// timer wakes the lane at reset and messages arriving meanwhile queue
/// instead of burning against the rejected backend.
///
/// Returning the armed state TOGETHER with its announcement line is the
/// point: these arms used to log "parked" while arming nothing (live
/// 2026-07-29, sessions 379864df/a43b7f32 — the reset never woke them,
/// the credential reload's park-cancel found nothing to resume, and
/// their interrupted work was silently lost). A caller of this
/// constructor cannot log the line without holding the park it
/// announces, and the line's `has_pending` flavor is derived from the
/// pending it ships with, so the two can never diverge.
pub(crate) fn backend_started_limit_park(
    resets_at_epoch: Option<u64>,
    now: tokio::time::Instant,
    now_epoch: u64,
    streak: u32,
    jitter_secs: u64,
    turn_had_started: bool,
) -> (LimitParkState, String) {
    let delay = limit_park_delay(resets_at_epoch, now_epoch, streak, jitter_secs);
    let pending = midturn_continuation(LIMIT_MIDTURN_CONTINUATION_TEXT, turn_had_started);
    let park_line = limit_park_log_line(resets_at_epoch, now_epoch, pending.is_some());
    (
        LimitParkState {
            resume_at: now + delay,
            pending,
            kind: ParkKind::ProviderLimit,
        },
        park_line,
    )
}

// ---------------------------------------------------------------------------
// Service-condition error park (recovery scheduling for early round endings)
// ---------------------------------------------------------------------------

/// The bounded widening wake schedule for a round killed by a temporary
/// service condition, indexed by consecutive-death `attempt` (1-based):
/// short first (a provider blip recovers in seconds), then longer, so a
/// real incident is retried patiently. Past the last entry the schedule
/// is EXHAUSTED and the lane suspends visibly instead of parking again
/// ([`error_park_attempts_exhausted`]). Integers deliberately tunable.
pub(crate) const ERROR_PARK_BACKOFF_SCHEDULE_SECS: [u64; 5] = [30, 120, 300, 900, 1800];
/// Jitter cap stacked on each schedule step so a fleet whose sessions
/// all died on the same provider incident doesn't wake in lockstep.
/// Deliberately smaller than the rate-limit park's 30–90s band — the
/// first schedule step is only 30s.
pub(crate) const ERROR_PARK_JITTER_MAX_SECS: u64 = 15;

/// Random error-park jitter in `0..=ERROR_PARK_JITTER_MAX_SECS`. Tests
/// inject a fixed value into [`error_park_delay`] instead of calling this.
pub(crate) fn error_park_jitter_secs() -> u64 {
    use rand::Rng;
    rand::thread_rng().gen_range(0..=ERROR_PARK_JITTER_MAX_SECS)
}

/// The wake delay for recovery `attempt` (1-based). Attempts past the
/// schedule clamp to its last step — but callers gate on
/// [`error_park_attempts_exhausted`] first, so the clamp only matters if
/// a caller deliberately keeps retrying past exhaustion.
pub(crate) fn error_park_delay(attempt: u32, jitter_secs: u64) -> Duration {
    let index = (attempt.max(1) as usize - 1).min(ERROR_PARK_BACKOFF_SCHEDULE_SECS.len() - 1);
    Duration::from_secs(ERROR_PARK_BACKOFF_SCHEDULE_SECS[index].saturating_add(jitter_secs))
}

/// Whether recovery `attempt` (1-based) lies past the bounded schedule.
/// The first exhausted attempt is `len + 1`: every entry in the schedule
/// buys one real wake before the lane gives up visibly.
pub(crate) fn error_park_attempts_exhausted(attempt: u32) -> bool {
    attempt as usize > ERROR_PARK_BACKOFF_SCHEDULE_SECS.len()
}

/// The resume nudge a service-condition error park re-sends at wake when
/// the killed round had already started at the backend (the delivery-aware
/// twin of [`LIMIT_MIDTURN_CONTINUATION_TEXT`], through the same
/// [`delivery_aware_park_pending`] seam).
pub(crate) const ERROR_MIDTURN_CONTINUATION_TEXT: &str =
    "A temporary service error interrupted the previous turn (the provider returned repeated \
     server errors); the recovery pause has elapsed — continue where you left off. Do not \
     expect the interrupted message to be re-sent: it is already in this conversation.";

/// Classify a fatal round-ending backend error: `true` means a TEMPORARY
/// service condition — the provider-incident class (HTTP 5xx after the
/// backend's own transport retries gave up, gateway drops, stream cuts) —
/// whose round death should arm the error park instead of ending the
/// round's story. Everything else (auth problems, refusals, invalid
/// pins, budget stops, deliberate exits) is PERMANENT: those endings
/// keep today's terminal shapes. Matching is deliberately conservative
/// and marker-based; the observed shapes:
///
/// - Claude Code surfaces provider errors as a result whose text starts
///   "API Error: <status>" (the 2026-07-29 specimens: "API Error: 500",
///   sessions 24f01636/13e53300) — `api error: 5` covers the 5xx band
///   without matching auth statuses (401/403) or limits (429).
/// - Anthropic 529s carry "overloaded"; generic 5xx bodies carry
///   "internal server error" / "bad gateway" / "service unavailable" /
///   "gateway timeout".
/// - Codex labels stream cuts `streamDisconnected` with "stream
///   disconnected before completion" prose; raw socket deaths surface
///   "connection reset" / "fetch failed".
pub(crate) fn transient_service_condition(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    [
        "api error: 5",
        "overloaded",
        "internal server error",
        "bad gateway",
        "service unavailable",
        "gateway timeout",
        "stream disconnected",
        "streamdisconnected",
        "connection reset",
        "fetch failed",
    ]
    .iter()
    .any(|marker| reason.contains(marker))
}

/// Classify a fatal round-ending backend error as the PROVIDER-SAFEGUARDS
/// class: the provider's safety layer flagged the conversation and refused
/// to continue it. This class is TERMINAL FOR THOSE BYTES — the flag
/// judges the conversation's content, so mechanically retrying, parking,
/// or resuming the same context re-flags forever (proven live 2026-07-31:
/// a resume into a flagged context re-flagged immediately, three times in
/// one arc — sessions 69c8535e/77c8beaf). House law (owner, standing):
/// never fall back to another model without per-instance owner approval;
/// the remedy is a FRESH session with the task RECAST in the owner's own
/// words — a judgment act, not mechanics. This classifier outranks
/// [`transient_service_condition`] in the round-outcome ladder and feeds
/// the honest terminal (the safeguards-flagged chip and attention
/// surfaces) and the recovery guards (boot readopt and the commission
/// sweep LIST, never nudge, this class). It must never feed a retry lane.
///
/// Matching is deliberately conservative and marker-based, like its
/// transient twin. The classifier is the one provider-general seam —
/// other providers' flag shapes join this list as real specimens arrive,
/// never speculatively. The observed shapes:
///
/// - Claude Code surfaces the Anthropic flag as a result whose text
///   reads "API Error: <model>'s safeguards flagged this message
///   (https://www.anthropic.com/legal/aup)…" (474-byte specimen,
///   byte-identical across four 2026-07-31 firings) — "safeguards
///   flagged" covers it, and the AUP URL also covers wording drift
///   around the verb.
/// - "flagged by our safeguards" is the same family's other phrasing
///   ("Claude's response was flagged by our safeguards").
/// - The API's structured refusal carries `stop_details.explanation`
///   prose "…blocked under Anthropic's Usage Policy…" — matched for
///   adapters that surface the structured explanation instead of the
///   CLI banner.
pub(crate) fn safeguards_flag_condition(reason: &str) -> bool {
    let reason = reason.to_ascii_lowercase();
    [
        "safeguards flagged",
        "flagged by our safeguards",
        "anthropic.com/legal/aup",
        "blocked under anthropic's usage policy",
    ]
    .iter()
    .any(|marker| reason.contains(marker))
}

/// The cause of a fatal, non-recoverable backend error buffered while the
/// drain waits out the post-turn grace window. `reason` is the formatted
/// announcement (agent name + code + message — what logs and outcomes
/// carry); `raw_message` is the adapter's verbatim error text, kept so
/// the drain can recognize the error's OWN synthesized turn completion
/// (Claude Code pushes `TurnCompleted` carrying the same text right after
/// the fatal `BackendError`, one wire line) and not mistake it for a real
/// completion superseding the cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FatalRoundError {
    pub(crate) reason: String,
    pub(crate) raw_message: String,
}

/// First line of a (possibly multi-line, JSON-bearing) backend error,
/// truncated for one-row announcements. The full cause is already in the
/// session log as the drain's own error row.
fn error_reason_preview(reason: &str) -> String {
    let first_line = reason.lines().next().unwrap_or("").trim();
    truncate_string_copy(first_line, 160)
}

/// The session-log/activity row announcing a service-condition park —
/// the error-park twin of [`limit_park_log_line`], one place so the
/// lanes cannot drift.
pub(crate) fn error_park_log_line(
    reason: &str,
    attempt: u32,
    delay: Duration,
    has_pending: bool,
) -> String {
    let tail = if has_pending {
        "will auto-resume and continue the interrupted work (messages arriving meanwhile queue)"
    } else {
        "messages arriving meanwhile queue until the pause elapses"
    };
    format!(
        "Temporary service condition ended the round ({}) — parked; recovery attempt {attempt} of {} wakes in ~{}m{}s; {tail}",
        error_reason_preview(reason),
        ERROR_PARK_BACKOFF_SCHEDULE_SECS.len(),
        delay.as_secs() / 60,
        delay.as_secs() % 60,
    )
}

/// The visible-suspension announcement when the widening schedule is
/// exhausted: the lane stops parking and ends/reports FAILED so the
/// outage surfaces (agenda occurrences journal `failed` and count on the
/// suspension streak) instead of waiting unattended.
pub(crate) fn error_park_exhausted_line(reason: &str, attempts_made: u32) -> String {
    format!(
        "Temporary service condition persisted through {attempts_made} recovery attempts — \
         suspending instead of waiting unattended: {}",
        error_reason_preview(reason),
    )
}

/// The armed service-condition park PLUS its announcement line — the
/// error-park twin of [`backend_started_limit_park`]'s (park, line)
/// unity: a caller cannot log the park without holding the state it
/// announces, and the line's flavor derives from the pending it ships
/// with. `rejected` is the round's driving message when this side sent
/// one (the follow-up lane; delivery-aware verbatim re-send when the
/// backend never started the turn) and `None` for backend-started
/// rounds (nothing of ours to re-send; the pending is the resume nudge
/// exactly when the turn had started). Both shapes ride the one
/// [`delivery_aware_park_pending`] / [`midturn_continuation`] seam.
pub(crate) fn transient_round_death_error_park(
    reason: &str,
    now: tokio::time::Instant,
    attempt: u32,
    jitter_secs: u64,
    turn_had_started: bool,
    rejected: Option<FollowUpMessage>,
) -> (LimitParkState, String) {
    let delay = error_park_delay(attempt, jitter_secs);
    let pending = match rejected {
        Some(rejected) => Some(delivery_aware_park_pending(
            rejected,
            turn_had_started,
            ERROR_MIDTURN_CONTINUATION_TEXT,
        )),
        None => midturn_continuation(ERROR_MIDTURN_CONTINUATION_TEXT, turn_had_started),
    };
    let park_line = error_park_log_line(reason, attempt, delay, pending.is_some());
    (
        LimitParkState {
            resume_at: now + delay,
            pending,
            kind: ParkKind::ServiceCondition,
        },
        park_line,
    )
}

/// The queued-while-parked row for a user follow-up held during a park.
pub(crate) const LIMIT_PARK_QUEUED_MESSAGE_LOG: &str =
    "Message queued — delivers when the limit resets";

/// Deferral for an out-of-band `/compact` requested while a rate-limit
/// park is armed: dispatching it would burn against the very limit the
/// park is waiting out (observed live 2026-07-17 — repeated compaction
/// attempts into a #388 park each answered "You've hit your session
/// limit"). Both idle lanes (the supervised external-mode loop and the
/// persistent daemon lane) skip the dispatch and answer with this calm
/// line instead; the requester re-runs `/compact` after the reset.
/// Returns `None` when the action may dispatch (not a compact, or no
/// park armed). Pure — clock injected for tests.
pub(crate) fn compact_deferred_by_limit_park(
    action: &str,
    limit_park: &Option<LimitParkState>,
    now: tokio::time::Instant,
    now_epoch: u64,
) -> Option<String> {
    if action != "compact" {
        return None;
    }
    let park = limit_park.as_ref()?;
    let resets_at_epoch =
        now_epoch.saturating_add(park.resume_at.saturating_duration_since(now).as_secs());
    let phrase = external_agent::limit_reset_phrase(Some(resets_at_epoch), now_epoch);
    Some(match park.kind {
        ParkKind::ProviderLimit => format!(
            "Compaction deferred — rate-limited; {phrase}; request it again after the limit resets",
        ),
        ParkKind::ServiceCondition => format!(
            "Compaction deferred — waiting out a service condition; {phrase}; request it again after the pause elapses",
        ),
    })
}

/// Pop the next still-deliverable message off a rate-limit park queue,
/// dropping entries cancelled while they waited. FIFO — the pending
/// re-send sits at the front, user messages queued during the park
/// behind it. Returns the message plus how many cancelled entries were
/// skipped (for the caller's log row). Shared by both lanes so the
/// resume-flush semantics cannot drift.
pub(crate) fn next_parked_follow_up(
    parked: &mut std::collections::VecDeque<FollowUpMessage>,
    cancelled_follow_ups: &mut HashSet<String>,
) -> (Option<FollowUpMessage>, usize) {
    let mut skipped = 0usize;
    while let Some(queued) = parked.pop_front() {
        if follow_up_message_was_cancelled(cancelled_follow_ups, &queued) {
            skipped += 1;
            continue;
        }
        return (Some(queued), skipped);
    }
    (None, skipped)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalBackendRecovery {
    pub(crate) message: String,
    pub(crate) recovery_hint: Option<String>,
}

/// Build the control plane's live Claude Code runtime config from the
/// project TOML. Mirrors the inline Codex/Gemini seeding blocks.
pub(crate) fn shared_claude_config_from_project(
    project: &Project,
) -> control_plane::SharedClaudeConfig {
    let cfg = &project.config.agent.claude_code;
    Arc::new(tokio::sync::RwLock::new(
        control_plane::ClaudeRuntimeConfig {
            model: cfg.model.clone(),
            effort: project::normalize_claude_effort(cfg.effort.as_deref()),
            permission_mode: project::normalize_claude_permission_mode(&cfg.permission_mode),
            allowed_tools: cfg.allowed_tools.clone(),
        },
    ))
}

pub(crate) fn shared_kimi_config_from_project(
    project: &Project,
) -> control_plane::SharedKimiConfig {
    let cfg = &project.config.agent.kimi;
    Arc::new(tokio::sync::RwLock::new(control_plane::KimiRuntimeConfig {
        command: cfg.command.clone(),
        model: cfg.model.clone(),
        thinking: project::normalize_kimi_thinking(cfg.thinking.as_deref()),
        permission_mode: project::normalize_kimi_permission_mode(&cfg.permission_mode),
        allowed_tools: cfg.allowed_tools.clone(),
        plan_mode: cfg.plan_mode,
        swarm_mode: cfg.swarm_mode,
    }))
}

/// Live Codex config for the control plane — seeded from TOML, updated
/// by SetCodex* ControlMsgs. The daemon loop and mode branches read
/// this at the start of each task so a Control-tab toggle takes effect
/// on the next task without a restart. (Twin of
/// shared_claude_config_from_project above; was four inline copies in
/// the mode branches before the wiring dedup.)
pub(crate) fn shared_codex_config_from_project(
    project: &Project,
) -> control_plane::SharedCodexConfig {
    let cfg = &project.config.agent.codex;
    Arc::new(tokio::sync::RwLock::new(
        control_plane::CodexRuntimeConfig {
            command: cfg.command.clone(),
            managed_command: cfg.managed_command.clone(),
            sandbox: project::normalize_sandbox_mode(&cfg.sandbox),
            approval_policy: project::normalize_approval_policy(&cfg.approval_policy),
            model: cfg.model.clone(),
            reasoning_effort: project::normalize_reasoning_effort(cfg.reasoning_effort.as_deref()),
            service_tier: project::normalize_codex_service_tier(cfg.service_tier.as_deref()),
            web_search: cfg.web_search,
            network_access: cfg.network_access,
            writable_roots: cfg.writable_roots.clone(),
            managed_context: project::normalize_codex_managed_context(&cfg.managed_context),
            context_archive: project::normalize_codex_context_archive(&cfg.context_archive),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The park-then-die hold line: honest, greppable, and truthful with
    /// and without a carried reason.
    #[test]
    fn park_holds_through_terminal_line_is_honest() {
        let line = park_holds_through_terminal_line(
            ParkKind::ProviderLimit,
            "the observed round failed before any turn completed",
            "You've hit your session limit · resets 1:40am",
        );
        assert_eq!(
            line,
            "Rate-limit park holds through a backend terminal while parked — the observed \
             round failed before any turn completed (You've hit your session limit · resets \
             1:40am); the session stays resident and resumes at the park's wake"
        );
        let quiet = park_holds_through_terminal_line(
            ParkKind::ServiceCondition,
            "the backend event channel closed",
            "",
        );
        assert_eq!(
            quiet,
            "Service-recovery pause holds through a backend terminal while parked — the \
             backend event channel closed; the session stays resident and resumes at the \
             park's wake"
        );
    }

    /// The reload-outranks-terminal line: honest, greppable, and
    /// truthful with and without a carried reason (the limit-exit race
    /// rescue's log row, card 01KZ0HVNCM).
    #[test]
    fn reload_outranks_terminal_line_is_honest() {
        let line = reload_outranks_terminal_line(
            "the round failed before any turn completed",
            "Claude Code auth refused",
        );
        assert_eq!(
            line,
            "Deferred credential reload outranks a backend terminal — the round failed \
             before any turn completed (Claude Code auth refused); the session stays \
             resident and respawns on the fresh credential store"
        );
        let quiet = reload_outranks_terminal_line("the backend event channel closed", "");
        assert_eq!(
            quiet,
            "Deferred credential reload outranks a backend terminal — the backend event \
             channel closed; the session stays resident and respawns on the fresh \
             credential store"
        );
    }

    /// Re-arming over an armed park inherits the owed pending: while
    /// parked nothing delivers user input, so the earlier park's pending
    /// is still the owed work and outranks a replacement arm's
    /// synthesized nudge (or its absence).
    #[test]
    fn inherit_owed_pending_preserves_the_owed_message() {
        let owed = FollowUpMessage::text("the owed re-send".to_string())
            .with_follow_up_id(Some("f-owed".to_string()));
        let previous = LimitParkState {
            resume_at: tokio::time::Instant::now(),
            pending: Some(owed),
            kind: ParkKind::ProviderLimit,
        };

        // Replacement armed with only a synthesized nudge: the owed
        // message wins.
        let (mut park, _) = backend_started_limit_park(
            Some(2_000_000_000),
            tokio::time::Instant::now(),
            1_999_999_000,
            1,
            0,
            true,
        );
        assert!(park.pending.is_some(), "nudge-armed replacement");
        assert!(inherit_owed_pending(Some(previous), &mut park));
        let pending = park.pending.expect("owed pending inherited");
        assert_eq!(pending.text, "the owed re-send");
        assert_eq!(pending.follow_up_id.as_deref(), Some("f-owed"));

        // No previous park: the replacement keeps its own pending.
        let (mut park, _) = backend_started_limit_park(
            Some(2_000_000_000),
            tokio::time::Instant::now(),
            1_999_999_000,
            1,
            0,
            true,
        );
        assert!(!inherit_owed_pending(None, &mut park));
        assert_eq!(
            park.pending.map(|p| p.text),
            Some(LIMIT_MIDTURN_CONTINUATION_TEXT.to_string())
        );

        // Previous park without pending: nothing carries over, the
        // pending-less replacement stays pending-less.
        let pending_less = LimitParkState {
            resume_at: tokio::time::Instant::now(),
            pending: None,
            kind: ParkKind::ProviderLimit,
        };
        let (mut park, _) = backend_started_limit_park(
            Some(2_000_000_000),
            tokio::time::Instant::now(),
            1_999_999_000,
            1,
            0,
            false,
        );
        assert!(park.pending.is_none());
        assert!(!inherit_owed_pending(Some(pending_less), &mut park));
        assert!(park.pending.is_none());
    }

    /// The waiting-status vocabulary is shared by the arm sites and the
    /// held-through-terminal re-emits — one source, no drift.
    #[test]
    fn waiting_turn_status_vocabulary_is_pinned() {
        assert_eq!(
            ParkKind::ProviderLimit.waiting_turn_status(),
            "waiting-rate-limit"
        );
        assert_eq!(
            ParkKind::ServiceCondition.waiting_turn_status(),
            "waiting-service-recovery"
        );
        assert_eq!(
            ParkKind::ProviderLimit.waiting_turn_detail("claude-code"),
            "claude-code rate-limited; parked until the limit resets"
        );
        assert_eq!(
            ParkKind::ServiceCondition.waiting_turn_detail("claude-code"),
            "claude-code waiting out a temporary service condition; parked for recovery"
        );
    }

    /// THE no-auto-re-execution pin for the died-with-restart class: the
    /// marking seam flips registry records and publishes SURFACES only —
    /// a log row and the honest activity snapshot. It never emits a
    /// dispatch-shaped event, never constructs a FollowUpMessage, and the
    /// re-run OFFER composer mints nothing on its own (`None` without
    /// died tasks; text-only with them). Anyone adding a re-run lane for
    /// this class must delete this pin first.
    #[test]
    fn died_with_restart_marking_never_arms_delivery() {
        let sid = "cc-died-pin-session";
        crate::background_tasks::clear_session(sid);
        crate::background_tasks::record_started(
            sid,
            "task-1",
            "toolu_1",
            "cargo test battery",
            100,
        );
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let dir = tempfile::tempdir().unwrap();
        let log = std::sync::Arc::new(std::sync::Mutex::new(
            session_log::SessionLog::open(dir.path().join("s")).unwrap(),
        ));
        slog(&log, |l| l.write_meta(None, Some("seat")));

        let descs = mark_parked_tasks_died_with_restart(
            &bus,
            &log,
            &Some("wrapper-1".to_string()),
            Some(sid),
            SERVICE_RECOVERY_RESTART_CAUSE,
        );
        assert_eq!(descs, vec!["cargo test battery".to_string()]);

        // The registry flipped with the named cause and RETAINED the row.
        let task = crate::background_tasks::find_task(sid, "task-1").expect("retained");
        assert_eq!(
            task.status,
            crate::background_tasks::BackgroundTaskStatus::DiedWithRestart
        );
        assert_eq!(
            task.died_cause.as_deref(),
            Some(SERVICE_RECOVERY_RESTART_CAUSE)
        );

        // The durable marker flipped to its died form.
        let meta_park = log.lock().unwrap().dir().join("session_meta.json");
        let meta: session_log::SessionMeta =
            serde_json::from_str(&std::fs::read_to_string(meta_park).unwrap()).unwrap();
        let park = meta.bg_park.expect("died marker stamped");
        assert_eq!(
            park.died_cause.as_deref(),
            Some(SERVICE_RECOVERY_RESTART_CAUSE)
        );
        assert_eq!(park.tasks, vec!["cargo test battery".to_string()]);

        // THE PIN: the bus carries surfaces only. No follow-up, steer,
        // task-start, or any other dispatch-shaped event exists on it.
        let mut saw_log = false;
        let mut saw_activity = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::LogEntry { .. } => saw_log = true,
                AppEvent::SessionActivity { activity, .. } => {
                    saw_activity = true;
                    assert_eq!(
                        activity.died_background_tasks,
                        vec!["cargo test battery".to_string()]
                    );
                    assert_eq!(
                        activity.died_tasks_cause.as_deref(),
                        Some(SERVICE_RECOVERY_RESTART_CAUSE)
                    );
                    assert_eq!(activity.state, crate::types::SessionActivityState::Idle);
                    assert!(activity.background_tasks.is_empty());
                }
                other => panic!(
                    "died-with-restart marking emitted a non-surface event: {other:?} — \
                     no automatic re-execution lane may exist for this class"
                ),
            }
        }
        assert!(saw_log && saw_activity, "both surfaces publish");

        // The offer composer never mints a message of its own.
        assert!(died_tasks_nudge_addendum(&[], SERVICE_RECOVERY_RESTART_CAUSE).is_none());
        let one = died_tasks_nudge_addendum(
            &["cargo test battery".to_string()],
            SERVICE_RECOVERY_RESTART_CAUSE,
        )
        .expect("addendum text");
        assert!(one.contains("NOT re-run automatically"), "{one}");
        assert!(one.contains(SERVICE_RECOVERY_RESTART_CAUSE), "{one}");

        // Marking with nothing running is silent — no surfaces, no churn.
        let again = mark_parked_tasks_died_with_restart(
            &bus,
            &log,
            &Some("wrapper-1".to_string()),
            Some(sid),
            RATE_LIMIT_RESTART_CAUSE,
        );
        assert!(again.is_empty());
        assert!(rx.try_recv().is_err(), "no re-mark, no surfaces");
        crate::background_tasks::clear_session(sid);
    }

    /// THE re-arm pin for the ScheduleWakeup respawn variant (the
    /// 2026-08-01 specimen: a session parked on NOTHING but its harness
    /// timer): the marking seam takes a pending native wakeup over even
    /// when zero background tasks died — the registry record flips
    /// wrapper-owned under the named cause, the durable marker mirrors
    /// the re-armed form, and the one surface is a log row. No delivery
    /// is armed here: the supervising loop's deadline arm owns the wake.
    /// Later seams find the record already wrapper-owned and stay silent
    /// (the first, most specific cause stands).
    #[test]
    fn native_wakeup_takeover_rearms_without_died_tasks() {
        let sid = "cc-wakeup-takeover-session";
        crate::native_wakeup::record_armed(
            sid,
            crate::native_wakeup::NativeWakeupRecord {
                armed_at_epoch: 100,
                fire_at_epoch: 880,
                prompt: "<<autonomous-loop-dynamic>>".into(),
                reason: Some("watching the merge queue".into()),
                tool_use_id: "toolu_wk1".into(),
                rearmed_cause: None,
            },
        );
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let dir = tempfile::tempdir().unwrap();
        let log = std::sync::Arc::new(std::sync::Mutex::new(
            session_log::SessionLog::open(dir.path().join("s")).unwrap(),
        ));
        slog(&log, |l| l.write_meta(None, Some("seat")));

        let descs = mark_parked_tasks_died_with_restart(
            &bus,
            &log,
            &Some("wrapper-wk".to_string()),
            Some(sid),
            SERVICE_RECOVERY_RESTART_CAUSE,
        );
        assert!(descs.is_empty(), "no background tasks died");

        let record = crate::native_wakeup::pending_for(sid).expect("record retained");
        assert_eq!(
            record.rearmed_cause.as_deref(),
            Some(SERVICE_RECOVERY_RESTART_CAUSE)
        );

        let meta_path = log.lock().unwrap().dir().join("session_meta.json");
        let meta: session_log::SessionMeta =
            serde_json::from_str(&std::fs::read_to_string(meta_path).unwrap()).unwrap();
        let marker = meta.native_wakeup.expect("re-armed marker stamped");
        assert_eq!(
            marker.rearmed_cause.as_deref(),
            Some(SERVICE_RECOVERY_RESTART_CAUSE)
        );
        assert!(marker.died_cause.is_none(), "re-armed, not lost");
        assert_eq!(marker.fire_at_epoch, 880);
        assert_eq!(marker.prompt, "<<autonomous-loop-dynamic>>");

        // Surfaces only: exactly the one info row, nothing
        // dispatch-shaped (the no-auto-delivery law shared with the
        // died-tasks pin above — delivery belongs to the loop's arm).
        let mut saw_rearm_line = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                AppEvent::LogEntry { content, level, .. } => {
                    assert_eq!(level, "info");
                    assert!(content.contains("re-armed"), "{content}");
                    assert!(
                        content.contains(SERVICE_RECOVERY_RESTART_CAUSE),
                        "{content}"
                    );
                    saw_rearm_line = true;
                }
                other => panic!("wakeup takeover emitted a non-surface event: {other:?}"),
            }
        }
        assert!(saw_rearm_line, "the re-arm announces itself");

        // Idempotent across later seams: first cause stands, no
        // re-announce.
        let again = mark_parked_tasks_died_with_restart(
            &bus,
            &log,
            &Some("wrapper-wk".to_string()),
            Some(sid),
            RATE_LIMIT_RESTART_CAUSE,
        );
        assert!(again.is_empty());
        assert!(rx.try_recv().is_err(), "already wrapper-owned: silent");
        assert_eq!(
            crate::native_wakeup::pending_for(sid)
                .unwrap()
                .rearmed_cause
                .as_deref(),
            Some(SERVICE_RECOVERY_RESTART_CAUSE)
        );
        crate::native_wakeup::consume(sid);
    }

    /// The lost-timer note composes like the died-task offer: nothing
    /// without a died cause (a pending or re-armed wake is not lost),
    /// text-only with one — carrying the cause, the model's own wake
    /// prompt, and the stated reason.
    #[test]
    fn died_wakeup_addendum_only_speaks_for_lost_timers() {
        let mut meta = session_log::SessionNativeWakeupMeta {
            armed_at_epoch: 100,
            fire_at_epoch: 880,
            prompt: "<<autonomous-loop-dynamic>>".into(),
            reason: Some("queue watch".into()),
            rearmed_cause: None,
            died_cause: None,
            died_at_epoch: None,
        };
        assert!(died_wakeup_nudge_addendum(&meta, 900).is_none());

        meta.died_cause = Some(DAEMON_RESTART_CAUSE.into());
        let note = died_wakeup_nudge_addendum(&meta, 900).expect("note");
        assert!(note.contains(DAEMON_RESTART_CAUSE), "{note}");
        assert!(note.contains("<<autonomous-loop-dynamic>>"), "{note}");
        assert!(note.contains("NOT re-armed"), "{note}");
        assert!(note.contains("(reason: queue watch)"), "{note}");

        // An empty prompt is said, not omitted.
        meta.prompt = String::new();
        assert!(died_wakeup_nudge_addendum(&meta, 900)
            .unwrap()
            .contains("(empty)"));
    }

    /// The durable bg-park marker follows the activity claims: parked
    /// stamps the live form, quiet idle clears only a live marker (a
    /// died statement survives a respawned backend settling), and any
    /// turn state clears everything — work demonstrably resumed.
    #[test]
    fn bg_park_marker_follows_activity_claims() {
        let dir = tempfile::tempdir().unwrap();
        let log = std::sync::Arc::new(std::sync::Mutex::new(
            session_log::SessionLog::open(dir.path().join("s")).unwrap(),
        ));
        slog(&log, |l| l.write_meta(None, Some("seat")));
        let meta_path = { log.lock().unwrap().dir().join("session_meta.json") };
        let read_park = || -> Option<session_log::SessionBgParkMeta> {
            serde_json::from_str::<session_log::SessionMeta>(
                &std::fs::read_to_string(&meta_path).unwrap(),
            )
            .unwrap()
            .bg_park
        };
        let activity = |state: crate::types::SessionActivityState,
                        tasks: Vec<String>|
         -> crate::types::SessionActivityVitals {
            crate::types::SessionActivityVitals {
                state,
                background_tasks: tasks,
                ..Default::default()
            }
        };
        use crate::types::SessionActivityState as S;

        // Parked claim stamps the live marker.
        stamp_bg_park_marker_from_activity(
            &log,
            &activity(S::ParkedOnTasks, vec!["cargo test".into()]),
        );
        let park = read_park().expect("live marker");
        assert!(park.died_cause.is_none());
        assert_eq!(park.tasks, vec!["cargo test".to_string()]);

        // Quiet idle clears the LIVE marker (tasks drained normally).
        stamp_bg_park_marker_from_activity(&log, &activity(S::Idle, Vec::new()));
        assert!(read_park().is_none());

        // A died statement survives quiet idle settling…
        slog(&log, |l| {
            l.set_bg_park(Some(session_log::SessionBgParkMeta {
                tasks: vec!["cargo test".into()],
                died_cause: Some(DAEMON_RESTART_CAUSE.to_string()),
                died_at_epoch: Some(100),
            }))
        });
        stamp_bg_park_marker_from_activity(&log, &activity(S::Idle, Vec::new()));
        assert!(
            read_park().is_some_and(|p| p.died_cause.is_some()),
            "idle settling never erases the died statement"
        );
        // …and clears on demonstrable work.
        stamp_bg_park_marker_from_activity(&log, &activity(S::AwaitingApi, Vec::new()));
        assert!(read_park().is_none(), "a turn state resolves the attention");
    }

    fn rejected_park_message() -> FollowUpMessage {
        let mut rejected = FollowUpMessage::text("the whole goal, re-merged".to_string());
        rejected.follow_up_id = Some("f-goal".to_string());
        rejected.steer_id = Some("s-goal".to_string());
        rejected.edit_user_turn_index = Some(3);
        rejected.edit_user_turn_revision = Some(1);
        rejected.edit_original_text = Some("original".to_string());
        rejected
            .claude_inplace_rewind_targets
            .push("uuid-1".to_string());
        rejected
    }

    /// Pin `full_resend_only_when_never_delivered`: an instant rejection
    /// (the backend never engaged the message) parks the rejected
    /// message verbatim — text, ids, and edit/rewind directives intact —
    /// because the delivery truly never happened.
    #[test]
    fn full_resend_only_when_never_delivered() {
        let rejected = rejected_park_message();
        let pending = limit_park_pending(rejected.clone(), false);
        assert_eq!(pending.text, rejected.text);
        assert_eq!(pending.follow_up_id, rejected.follow_up_id);
        assert_eq!(pending.steer_id, rejected.steer_id);
        assert_eq!(pending.edit_user_turn_index, Some(3));
        assert_eq!(
            pending.claude_inplace_rewind_targets,
            vec!["uuid-1".to_string()]
        );
    }

    /// Pin `resume_nudge_when_turn_had_started` (and
    /// `park_resend_is_delivery_aware`): a rejection that cut a turn the
    /// backend had already started parks a short resume nudge — the
    /// delivered message is already in the backend's conversation, so
    /// re-flushing it would double the goal (live specimen 2026-07-28,
    /// session 800e6f58). The nudge inherits the rejected message's
    /// follow-up/steer ids so the park-elapse cancel check still honors
    /// a user cancel, and drops attachments and edit/rewind directives —
    /// those were applied when the turn started.
    #[test]
    fn resume_nudge_when_turn_had_started() {
        let rejected = rejected_park_message();
        let pending = limit_park_pending(rejected, true);
        assert_eq!(pending.text, LIMIT_MIDTURN_CONTINUATION_TEXT);
        assert_eq!(pending.follow_up_id.as_deref(), Some("f-goal"));
        assert_eq!(pending.steer_id.as_deref(), Some("s-goal"));
        assert!(pending.attachments.items.is_empty());
        assert_eq!(pending.edit_user_turn_index, None);
        assert_eq!(pending.edit_user_turn_revision, None);
        assert_eq!(pending.edit_original_text, None);
        assert!(pending.claude_inplace_rewind_targets.is_empty());

        // The park-elapse cancel gate matches the nudge through the
        // inherited ids: cancelling the original message cancels the
        // resume, exactly like it cancels a full re-send.
        let mut cancelled: HashSet<String> = HashSet::from(["f-goal".to_string()]);
        assert!(crate::external_events::follow_up_message_was_cancelled(
            &mut cancelled,
            &pending
        ));
    }

    /// Both mid-turn continuation lanes ride [`midturn_continuation`]:
    /// the started-turn decision ("nudge, never a re-send") has one home
    /// for the rate-limit park and the credential-reload respawn alike.
    #[test]
    fn midturn_continuation_is_the_shared_started_turn_seam() {
        assert!(midturn_continuation(LIMIT_MIDTURN_CONTINUATION_TEXT, false).is_none());
        let nudge = midturn_continuation(LIMIT_MIDTURN_CONTINUATION_TEXT, true)
            .expect("a started turn synthesizes a continuation");
        assert_eq!(nudge.text, LIMIT_MIDTURN_CONTINUATION_TEXT);
        assert!(nudge.follow_up_id.is_none());
        assert!(nudge.steer_id.is_none());
    }

    /// Pin `backend_started_limit_arms_real_park`: a backend-started
    /// round rejected at the provider limit arms a REAL
    /// [`LimitParkState`] — resume-nudge pending when the turn had
    /// started, a pending-less park (timer and queueing still armed)
    /// when it never did. The 2026-07-29 incident class logged "parked"
    /// from this lane while arming nothing, so neither the reset timer
    /// nor the credential reload could ever resume the session.
    #[tokio::test]
    async fn backend_started_limit_arms_real_park() {
        let now = tokio::time::Instant::now();
        let now_epoch = 1_000;
        let resets_at = Some(now_epoch + 3_600);

        let (park, _line) = backend_started_limit_park(resets_at, now, now_epoch, 1, 30, true);
        let pending = park.pending.as_ref().expect("started turn parks a nudge");
        assert_eq!(pending.text, LIMIT_MIDTURN_CONTINUATION_TEXT);
        assert!(pending.follow_up_id.is_none());
        assert!(pending.steer_id.is_none());
        assert_eq!(
            park.resume_at,
            now + limit_park_delay(resets_at, now_epoch, 1, 30)
        );

        // A rejection observed before any work still arms the park — no
        // work is owed at reset, but the timer wakes the lane and
        // messages arriving meanwhile queue instead of burning.
        let (park, _line) = backend_started_limit_park(resets_at, now, now_epoch, 1, 30, false);
        assert!(park.pending.is_none());
        assert_eq!(
            park.resume_at,
            now + limit_park_delay(resets_at, now_epoch, 1, 30)
        );
    }

    /// Pin `parked_log_line_implies_armed_state`: the constructor is the
    /// unity seam — the "parked" announcement exists only as the second
    /// half of a `(park, line)` pair, and the line's `has_pending`
    /// flavor is derived from the pending the park actually ships with.
    /// A log row claiming "parked" with nothing armed (the silent-loss
    /// bug class) cannot be constructed through this seam.
    #[tokio::test]
    async fn parked_log_line_implies_armed_state() {
        let now = tokio::time::Instant::now();
        let now_epoch = 1_000;
        let resets_at = Some(now_epoch + 3_600);

        for turn_had_started in [true, false] {
            let (park, line) =
                backend_started_limit_park(resets_at, now, now_epoch, 1, 30, turn_had_started);
            assert_eq!(
                line,
                limit_park_log_line(resets_at, now_epoch, park.pending.is_some()),
                "the announced flavor must match the armed pending"
            );
            assert!(line.starts_with("Rate-limited — parked"), "line: {line}");
        }
        let (_park, line) = backend_started_limit_park(resets_at, now, now_epoch, 1, 30, true);
        assert!(
            line.contains("will auto-resume and re-send the pending message"),
            "a nudge-armed park announces the re-send: {line}"
        );
        let (_park, line) = backend_started_limit_park(resets_at, now, now_epoch, 1, 30, false);
        assert!(
            line.contains("messages arriving meanwhile queue until the limit resets"),
            "a pending-less park announces queue-until-reset: {line}"
        );
    }

    /// Pin `limit_reset_wakes_backend_started_parks`: the armed pending
    /// is exactly what the idle select's park-elapse branch re-sends —
    /// the nudge carries no follow-up/steer ids, so an unrelated cancel
    /// recorded during the park cannot drop it, and the interrupted
    /// work is re-driven at reset.
    #[tokio::test]
    async fn limit_reset_wakes_backend_started_parks() {
        let now = tokio::time::Instant::now();
        let (park, _line) = backend_started_limit_park(Some(4_600), now, 1_000, 1, 30, true);

        // The park-elapse branch: deliver pending unless cancelled.
        let mut cancelled: HashSet<String> = HashSet::from(["f-unrelated".to_string()]);
        let pending = park.pending.expect("started turn parks a nudge");
        assert!(!crate::external_events::follow_up_message_was_cancelled(
            &mut cancelled,
            &pending
        ));
        assert_eq!(pending.text, LIMIT_MIDTURN_CONTINUATION_TEXT);
    }

    /// Pin `reload_resumes_backend_started_parks`: the credential
    /// reload's cancel-and-preserve push_fronts the armed pending, so a
    /// backend-started session parked at the limit resumes its
    /// interrupted work immediately after the respawn — ahead of
    /// messages queued during the park — exactly like the follow-up
    /// lane's parks behaved in the 2026-07-29 event (523c5c23/39dffb58
    /// resumed; the unparked 379864df idled forever).
    #[tokio::test]
    async fn reload_resumes_backend_started_parks() {
        let now = tokio::time::Instant::now();
        let (park, _line) = backend_started_limit_park(Some(4_600), now, 1_000, 1, 30, true);

        let mut parked_follow_ups: std::collections::VecDeque<FollowUpMessage> =
            std::collections::VecDeque::new();
        parked_follow_ups.push_back(FollowUpMessage::text("queued during park".to_string()));
        // `apply_backend_credentials_reload` (external_mode): take the
        // park, front-queue its pending, respawn; the flush preamble
        // then delivers FIFO.
        if let Some(pending) = park.pending {
            parked_follow_ups.push_front(pending);
        }
        let mut cancelled = HashSet::new();
        let (first, skipped) = next_parked_follow_up(&mut parked_follow_ups, &mut cancelled);
        assert_eq!(skipped, 0);
        assert_eq!(
            first.map(|m| m.text),
            Some(LIMIT_MIDTURN_CONTINUATION_TEXT.to_string())
        );
        let (second, _) = next_parked_follow_up(&mut parked_follow_ups, &mut cancelled);
        assert_eq!(
            second.map(|m| m.text),
            Some("queued during park".to_string())
        );
    }

    #[test]
    fn consumed_kimi_anchor_staging_is_not_a_durable_resume_pin() {
        let mut config = crate::session_config::SessionAgentConfig {
            forked_from: Some("session_parent".to_string()),
            fork_relationship: Some("anchor-fork".to_string()),
            fork_anchor: Some("{\"kind\":\"turn-boundary\",\"turn\":4}".to_string()),
            kimi_fork_rollback_turns: Some(2),
            kimi_fork_expected_horizon: Some("{\"active_turns\":6}".to_string()),
            ..Default::default()
        };

        clear_consumed_kimi_fork_staging(&mut config);

        assert_eq!(config.forked_from.as_deref(), Some("session_parent"));
        assert_eq!(config.fork_relationship.as_deref(), Some("anchor-fork"));
        assert!(config.fork_anchor.is_some());
        assert_eq!(config.kimi_fork_rollback_turns, None);
        assert_eq!(config.kimi_fork_expected_horizon, None);
    }

    #[test]
    fn buffered_idle_agent_event_preempts_and_disables_on_disconnect() {
        // A buffered event is returned (the flush must yield to it) …
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut event_rx = Some(rx);
        tx.send(external_agent::AgentEvent::TurnCompleted { message: None })
            .unwrap();
        assert!(matches!(
            try_buffered_idle_agent_event(&mut event_rx),
            Some(external_agent::AgentEvent::TurnCompleted { .. })
        ));
        // … an empty channel yields nothing and keeps the receiver armed …
        assert!(try_buffered_idle_agent_event(&mut event_rx).is_none());
        assert!(event_rx.is_some());
        // … and a closed channel disables the receiver like `recv() -> None`.
        drop(tx);
        assert!(try_buffered_idle_agent_event(&mut event_rx).is_none());
        assert!(event_rx.is_none());
        // A disabled receiver stays disabled.
        assert!(try_buffered_idle_agent_event(&mut event_rx).is_none());
    }

    /// A `/compact` arriving while a rate-limit park is armed defers with
    /// the calm reset-time line; anything else — other ops, or no park —
    /// dispatches normally. Clock injected: no sleeps, no wall time.
    #[tokio::test]
    async fn compact_thread_action_defers_while_limit_parked() {
        let now = tokio::time::Instant::now();
        let parked = Some(LimitParkState {
            resume_at: now + Duration::from_secs(10 * 60),
            pending: None,
            kind: ParkKind::ProviderLimit,
        });

        let deferral = compact_deferred_by_limit_park("compact", &parked, now, 1_000)
            .expect("parked compact must defer");
        assert!(
            deferral.starts_with("Compaction deferred — rate-limited"),
            "deferral: {deferral}"
        );
        assert!(deferral.contains("in ~10m"), "deferral: {deferral}");

        // A service-condition park defers the same compact with its own
        // honest wording (never "rate-limited").
        let error_parked = Some(LimitParkState {
            resume_at: now + Duration::from_secs(120),
            pending: None,
            kind: ParkKind::ServiceCondition,
        });
        let deferral = compact_deferred_by_limit_park("compact", &error_parked, now, 1_000)
            .expect("error-parked compact must defer");
        assert!(
            deferral.starts_with("Compaction deferred — waiting out a service condition"),
            "deferral: {deferral}"
        );

        // Not a compact: the park does not block other thread actions here.
        assert_eq!(
            compact_deferred_by_limit_park("fork", &parked, now, 1_000),
            None
        );
        // No park armed: compact dispatches.
        assert_eq!(
            compact_deferred_by_limit_park("compact", &None, now, 1_000),
            None
        );
    }

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

    #[test]
    fn limit_park_delay_targets_reset_plus_jitter() {
        // Reset 2h out, 60s jitter: park exactly until reset + jitter.
        let delay = limit_park_delay(Some(10_000 + 7_200), 10_000, 1, 60);
        assert_eq!(delay, Duration::from_secs(7_260));
        // A reset already in the past parks for just the jitter.
        let delay = limit_park_delay(Some(9_000), 10_000, 1, 45);
        assert_eq!(delay, Duration::from_secs(45));
        // A seven_day-style reset far out is capped to one re-check cycle.
        let delay = limit_park_delay(Some(10_000 + 3 * 24 * 3600), 10_000, 1, 30);
        assert_eq!(delay, Duration::from_secs(LIMIT_PARK_MAX_SECS + 30));
    }

    #[test]
    fn limit_park_delay_backs_off_exponentially_without_reset_time() {
        // 5 → 10 → 20 → 30 (capped) minutes; streak is 1-based and a
        // runaway streak must not overflow the shift.
        let d = |streak| limit_park_delay(None, 10_000, streak, 60).as_secs();
        assert_eq!(d(1), 5 * 60);
        assert_eq!(d(2), 10 * 60);
        assert_eq!(d(3), 20 * 60);
        assert_eq!(d(4), 30 * 60);
        assert_eq!(d(50), 30 * 60);
        // Streak 0 (defensive) behaves like the first park.
        assert_eq!(d(0), 5 * 60);
    }

    #[test]
    fn limit_park_jitter_stays_in_band() {
        for _ in 0..32 {
            let jitter = limit_park_jitter_secs();
            assert!((LIMIT_PARK_JITTER_MIN_SECS..=LIMIT_PARK_JITTER_MAX_SECS).contains(&jitter));
        }
    }

    #[test]
    fn parked_follow_ups_flush_fifo_and_honor_cancels() {
        let mut parked: std::collections::VecDeque<FollowUpMessage> =
            std::collections::VecDeque::new();
        let mut first = FollowUpMessage::text("re-send".to_string());
        first.follow_up_id = Some("fu-1".to_string());
        let mut cancelled_mid_park = FollowUpMessage::text("cancelled".to_string());
        cancelled_mid_park.follow_up_id = Some("fu-2".to_string());
        let mut last = FollowUpMessage::text("queued during park".to_string());
        last.follow_up_id = Some("fu-3".to_string());
        parked.push_back(first);
        parked.push_back(cancelled_mid_park);
        parked.push_back(last);

        // A cancel recorded while the message waited in the park queue.
        let mut cancelled: HashSet<String> = HashSet::new();
        cancelled.insert("fu-2".to_string());

        let (popped, skipped) = next_parked_follow_up(&mut parked, &mut cancelled);
        assert_eq!(popped.unwrap().text, "re-send");
        assert_eq!(skipped, 0);
        let (popped, skipped) = next_parked_follow_up(&mut parked, &mut cancelled);
        assert_eq!(
            popped.unwrap().text,
            "queued during park",
            "cancelled entries are dropped, later ones still deliver in order"
        );
        assert_eq!(skipped, 1);
        let (popped, skipped) = next_parked_follow_up(&mut parked, &mut cancelled);
        assert!(popped.is_none());
        assert_eq!(skipped, 0);
    }

    /// PIN `transient_round_death_arms_error_park`: a round death whose
    /// cause classifies as a temporary service condition (the 2026-07-29
    /// specimens: "API Error: 500" round-deaths that rode a DoneSignal
    /// and stranded both commissions fake-idle) arms a REAL park through
    /// the (park, line) unity constructor — armed timer, service kind,
    /// delivery-aware pending, and an announcement that cannot exist
    /// without the state it announces.
    #[tokio::test]
    async fn transient_round_death_arms_error_park() {
        let specimen = "Claude Code backend error (error_during_execution): API Error: 500 \
             {\"type\":\"api_error\",\"message\":\"Internal server error\"}";
        assert!(transient_service_condition(specimen));

        let now = tokio::time::Instant::now();
        // The specimen shape: a backend-started/mid-commission round the
        // backend had engaged — the pending is the resume nudge.
        let (park, line) = transient_round_death_error_park(specimen, now, 1, 0, true, None);
        assert_eq!(park.kind, ParkKind::ServiceCondition);
        assert_eq!(park.resume_at, now + error_park_delay(1, 0));
        let pending = park.pending.as_ref().expect("started turn parks a nudge");
        assert_eq!(pending.text, ERROR_MIDTURN_CONTINUATION_TEXT);
        assert!(
            line.contains("parked"),
            "announcement names the park: {line}"
        );
        assert!(
            line.contains("recovery attempt 1 of 5"),
            "announcement names the schedule position: {line}"
        );

        // A backend-started death observed before any work parks
        // pending-less — the timer still wakes the lane and messages
        // arriving meanwhile queue instead of burning.
        let (park, _line) = transient_round_death_error_park(specimen, now, 1, 0, false, None);
        assert!(park.pending.is_none());
        assert_eq!(park.kind, ParkKind::ServiceCondition);
    }

    /// PIN `error_park_wakes_on_backoff_expiry`: the error park's wake
    /// clock is the bounded widening schedule — short first, then longer
    /// — indexed by consecutive-death attempt, and the armed park's
    /// `resume_at` is exactly that delay out (the shared park timer
    /// branch re-sends the pending when it fires, same slot as the
    /// limit park's reset wake).
    #[tokio::test]
    async fn error_park_wakes_on_backoff_expiry() {
        // Widening: each schedule step is at least the previous one.
        let mut previous = 0u64;
        for (index, step) in ERROR_PARK_BACKOFF_SCHEDULE_SECS.iter().enumerate() {
            assert!(
                *step >= previous,
                "schedule must widen monotonically at step {index}"
            );
            previous = *step;
            assert_eq!(
                error_park_delay(index as u32 + 1, 0),
                Duration::from_secs(*step)
            );
        }
        // Short first: the first wake is prompt (a provider blip
        // recovers in seconds, not an hour).
        assert!(ERROR_PARK_BACKOFF_SCHEDULE_SECS[0] <= 60);
        // Jitter stacks on the step; attempts past the schedule clamp
        // to the last step.
        assert_eq!(
            error_park_delay(1, 7),
            Duration::from_secs(ERROR_PARK_BACKOFF_SCHEDULE_SECS[0] + 7)
        );
        assert_eq!(
            error_park_delay(99, 0),
            Duration::from_secs(*ERROR_PARK_BACKOFF_SCHEDULE_SECS.last().unwrap())
        );
        // The armed park wakes exactly on the schedule.
        let now = tokio::time::Instant::now();
        let (park, _line) = transient_round_death_error_park(
            "API Error: 503 Service Unavailable",
            now,
            3,
            5,
            true,
            None,
        );
        assert_eq!(park.resume_at, now + error_park_delay(3, 5));

        for _ in 0..32 {
            assert!(error_park_jitter_secs() <= ERROR_PARK_JITTER_MAX_SECS);
        }
    }

    /// PIN `exhausted_error_parks_suspend_visibly`: the schedule is
    /// attempt-capped — one wake per schedule step, and the first
    /// attempt PAST the schedule is exhausted. The lanes map exhaustion
    /// to a FAILED terminal (`TaskOutcome::Failed`) with this
    /// announcement instead of parking again, so a lasting outage
    /// journals `failed` on the scheduled occurrence — counting on the
    /// agenda's suspension streak and surfacing to the owner — rather
    /// than waiting unattended (the specimens waited over an hour,
    /// invisible to every wake clock).
    #[test]
    fn exhausted_error_parks_suspend_visibly() {
        let attempts = ERROR_PARK_BACKOFF_SCHEDULE_SECS.len() as u32;
        for attempt in 1..=attempts {
            assert!(
                !error_park_attempts_exhausted(attempt),
                "attempt {attempt} is within the schedule"
            );
        }
        assert!(error_park_attempts_exhausted(attempts + 1));

        let line = error_park_exhausted_line("API Error: 500 internal server error", attempts);
        assert!(
            line.contains("suspending"),
            "exhaustion announces a suspension, not a wait: {line}"
        );
        assert!(
            line.contains(&format!("{attempts} recovery attempts")),
            "exhaustion counts the schedule it burned: {line}"
        );
        assert!(line.contains("API Error: 500"));
    }

    /// PIN `error_park_shares_the_delivery_aware_seam`: the error park's
    /// pending decision IS the limit park's — one
    /// [`delivery_aware_park_pending`] / [`midturn_continuation`] seam,
    /// never a copy. Never-delivered → the driving message verbatim
    /// (ids, attachments, edit/rewind directives intact); started → a
    /// resume nudge naming the service condition, inheriting the
    /// follow-up/steer ids (a user cancel during the park cancels the
    /// resume) and carrying none of the applied directives.
    #[tokio::test]
    async fn error_park_shares_the_delivery_aware_seam() {
        // Never delivered: verbatim re-send, exactly like the limit twin.
        let rejected = rejected_park_message();
        let pending =
            delivery_aware_park_pending(rejected.clone(), false, ERROR_MIDTURN_CONTINUATION_TEXT);
        assert_eq!(pending.text, rejected.text);
        assert_eq!(pending.follow_up_id, rejected.follow_up_id);
        assert_eq!(pending.edit_user_turn_index, Some(3));
        assert_eq!(
            pending.claude_inplace_rewind_targets,
            vec!["uuid-1".to_string()]
        );

        // Started: the nudge, with inherited ids and nothing else.
        let pending = delivery_aware_park_pending(
            rejected_park_message(),
            true,
            ERROR_MIDTURN_CONTINUATION_TEXT,
        );
        assert_eq!(pending.text, ERROR_MIDTURN_CONTINUATION_TEXT);
        assert_eq!(pending.follow_up_id.as_deref(), Some("f-goal"));
        assert_eq!(pending.steer_id.as_deref(), Some("s-goal"));
        assert!(pending.attachments.items.is_empty());
        assert_eq!(pending.edit_user_turn_index, None);
        assert!(pending.claude_inplace_rewind_targets.is_empty());
        let mut cancelled: HashSet<String> = HashSet::from(["f-goal".to_string()]);
        assert!(crate::external_events::follow_up_message_was_cancelled(
            &mut cancelled,
            &pending
        ));

        // The limit park rides the identical seam — same function, its
        // own cause text — so the two lanes cannot drift.
        let limit_twin = limit_park_pending(rejected_park_message(), true);
        let shared = delivery_aware_park_pending(
            rejected_park_message(),
            true,
            LIMIT_MIDTURN_CONTINUATION_TEXT,
        );
        assert_eq!(limit_twin.text, shared.text);
        assert_eq!(limit_twin.follow_up_id, shared.follow_up_id);
        assert_eq!(limit_twin.steer_id, shared.steer_id);

        // The full-message re-send inherits the error text through the
        // constructor too: a never-started follow-up round parks the
        // driving message itself.
        let (park, _line) = transient_round_death_error_park(
            "API Error: 502 Bad Gateway",
            tokio::time::Instant::now(),
            1,
            0,
            false,
            Some(rejected_park_message()),
        );
        assert_eq!(
            park.pending.expect("driving message parks verbatim").text,
            "the whole goal, re-merged"
        );
    }
}
