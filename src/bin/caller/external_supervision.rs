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

pub(crate) fn emit_user_message_log(
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

pub(crate) fn json_string_field(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
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
        && a.permission_mode == b.permission_mode
        && a.allowed_tools == b.allowed_tools
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

pub(crate) fn read_diff_file_text(project_root: &Path, display_path: &str) -> Option<Option<String>> {
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

/// Construct, initialize, and start a thread for an external agent backend.
///
/// Returns the agent, thread handle, and event receiver. The caller owns the
/// agent lifetime and is responsible for sending messages and draining events.
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
    /// emitting `AgentStarted`. The presence path sets this to avoid
    /// duplicating the model reasoning that's already shown via ModelResponse.
    pub(crate) suppress_agent_started: bool,
    /// Supervised external sessions have their own session log in addition to
    /// the daemon's root log writer. Persist model responses here too so
    /// per-session replay does not depend on the root session log.
    pub(crate) persist_model_responses_inline: bool,
    /// When true and no `json_approval` slot is set, auto-deny approval
    /// requests (headless mode with no interactive input).
    pub(crate) headless: bool,
    /// Shared context-injection queue. Fallback target when the backend
    /// does not support mid-turn steering — queued items are drained on
    /// the next turn's follow-up message path.
    pub(crate) context_injection: &'a event::ContextInjectionQueue,
}

pub(crate) const EXTERNAL_CONTEXT_SNAPSHOT_INTERVAL: Duration = Duration::from_secs(1);

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
pub(crate) struct ExternalBackendRecovery {
    pub(crate) message: String,
    pub(crate) recovery_hint: Option<String>,
}

/// Build the control plane's live Claude Code runtime config from the
/// project TOML. Mirrors the inline Codex/Gemini seeding blocks.
pub(crate) fn shared_claude_config_from_project(project: &Project) -> control_plane::SharedClaudeConfig {
    let cfg = &project.config.agent.claude_code;
    Arc::new(tokio::sync::RwLock::new(
        control_plane::ClaudeRuntimeConfig {
            model: cfg.model.clone(),
            permission_mode: project::normalize_claude_permission_mode(&cfg.permission_mode),
            allowed_tools: cfg.allowed_tools.clone(),
        },
    ))
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
    Arc::new(tokio::sync::RwLock::new(control_plane::CodexRuntimeConfig {
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
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

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
}
