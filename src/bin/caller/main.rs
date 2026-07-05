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

use autonomy::{AutonomyLevel, AutonomyState, SharedAutonomy};
use conversation::Conversation;
use error::CallerError;
use event::{AppEvent, EventBus};
use project::Project;
use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{self, BufRead, IsTerminal, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
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

/// Build the [`peer::AuthRequirements`] this daemon advertises in
/// its own Agent Card from the project's `[server.auth]` config and
/// the access cert dir.
///
/// Resolution rules:
///
/// - `transport`:
///   - `advertised_transport = "none"` (default) → [`peer::TransportAuth::None`]
///   - `"mutual-tls"` → [`peer::TransportAuth::MutualTls`]
///   - `"pin-self-cert"` → read this daemon's own `server.crt` from
///     the access cert dir, compute its SHA-256 fingerprint, embed it
///     in [`peer::TransportAuth::PinnedMutualTls`]. Errors if no
///     cert is present (operator forgot to run `intendant access
///     setup`).
///   - any other value → config error
/// - `application`:
///   - `bearer_token = "..."` set → `Some(Bearer { hint, rotation_url: None })`
///     where `hint` documents where the token comes from so peers
///     can give operators a useful "configure me" message
///   - unset → `None`
///
/// Called once per spawn_web_gateway invocation, at daemon startup.
/// Errors propagate as `CallerError::Config` so the operator sees
/// a clean startup failure rather than a silent misconfigure.
fn build_local_advertised_auth(
    server_auth: &project::ServerAuthConfig,
    cert_dir: &std::path::Path,
) -> Result<peer::AuthRequirements, CallerError> {
    let transport = match server_auth.advertised_transport.as_str() {
        "none" => peer::TransportAuth::None,
        "mutual-tls" => peer::TransportAuth::MutualTls,
        "pin-self-cert" => {
            // `pin-self-cert` reads the local server cert produced by
            // `intendant access setup`. The cert store is per-user and is
            // consumed directly by native `--tls` / `--mtls`.
            let fp = access::certs::read_server_cert_fingerprint(cert_dir).ok_or_else(|| {
                CallerError::Config(format!(
                    "[server.auth] advertised_transport = \"pin-self-cert\" requires \
                     a local server cert at {}/server.crt — run `intendant access setup` \
                     first, or change advertised_transport to \"none\" / \"mutual-tls\"",
                    cert_dir.display()
                ))
            })?;
            peer::TransportAuth::PinnedMutualTls {
                server_cert_fingerprints: vec![fp],
            }
        }
        other => {
            return Err(CallerError::Config(format!(
                "[server.auth] advertised_transport = {other:?} is not a valid value \
                 (accepted: \"none\", \"mutual-tls\", \"pin-self-cert\")"
            )));
        }
    };
    let application = server_auth
        .bearer_token
        .as_ref()
        .map(|_| peer::ApplicationAuth::Bearer {
            hint: Some("[server.auth] bearer_token".to_string()),
            rotation_url: None,
        });
    Ok(peer::AuthRequirements {
        transport,
        application,
    })
}

/// Resolve the advertise-URL list passed to `spawn_web_gateway`,
/// applying CLI > config > auto-detect precedence.
///
/// - If `--advertise-url` was given (one or more times), the CLI list
///   wins entirely. The operator at the command line beats the
///   operator at the config file.
/// - Otherwise, if `[server.advertise]` in `intendant.toml` is non-
///   empty, that list is used.
/// - If both are empty, an empty `Vec` is returned, which signals
///   `spawn_web_gateway` to fall back to its single-URL auto-detection
///   from the listener's bind address (the historical behavior).
///
/// Returns owned `String`s so the caller can move the list directly
/// into `spawn_web_gateway` without an extra clone.
fn resolve_advertise_urls_from_flags_and_config(
    flags: &CliFlags,
    project: &Project,
) -> Vec<String> {
    if !flags.advertise_urls.is_empty() {
        flags.advertise_urls.clone()
    } else {
        project.config.server.advertise.clone()
    }
}

/// Build a peer registry for this daemon and hydrate it from the
/// `[[peer]]` sections in `intendant.toml`.
///
/// Spawns the durable log writer task (appending
/// `TaggedPeerEvent`s as JSONL to `<log_dir>/peers.jsonl`) and
/// creates a [`crate::peer::PeerRegistry`] wired to its sender.
/// Each config entry fires a background `add_peer` task so
/// slow/unreachable peers don't block daemon startup — the
/// registry's own reconnect state machine handles those
/// asynchronously once the card fetch returns.
///
/// The returned registry is cheaply cloneable (`Arc`-backed) and
/// gets passed into `spawn_web_gateway` so the `/api/peers`
/// handlers can inspect and mutate the same store. The log
/// writer's join handle is intentionally dropped — the writer
/// exits cleanly when all its senders go away (peer actors +
/// registry clones), and we don't currently have an explicit
/// daemon shutdown path that would await it.
fn build_and_hydrate_peer_registry(
    log_dir: &Path,
    peer_configs: &[project::PeerConfig],
) -> peer::PeerRegistry {
    let log_path = log_dir.join("peers.jsonl");
    let (log_tx, _log_handle) = peer::spawn_peer_log_writer(log_path);
    let registry = peer::PeerRegistry::new(log_tx);
    for cfg in peer_configs {
        let registry_for_task = registry.clone();
        let card_url = cfg.card_url.clone();
        let label = cfg.label.clone();
        let bearer_token = cfg.bearer_token.clone();
        let via_urls = cfg.via_urls.clone();
        let pinned_fingerprints = cfg.pinned_fingerprints.clone();
        let browser_tcp_via_url = cfg.browser_tcp_via_url.clone();
        let explicit_client_identity = match peer_client_identity_from_config(cfg) {
            Ok(identity) => identity,
            Err(e) => {
                eprintln!(
                    "intendant: failed to register peer from intendant.toml \
                     ({card_url}): {e}"
                );
                continue;
            }
        };
        tokio::spawn(async move {
            // via_urls, when non-empty, overrides the peer's self-advertised
            // transports. pinned_fingerprints, when non-empty, replaces the
            // card's auth.transport with
            // PinnedMutualTls — operator distrusts the card's claim
            // and pins against fingerprints they got out-of-band.
            // browser_tcp_via_url, when set, overrides the dashboard's
            // default `d.ws_url` fallback when opening WebRTC display
            // — used when the browser and primary can't share the
            // same URL (primary-side localhost tunnel, split
            // browser/primary machines, etc.).
            if let Err(e) = registry_for_task
                .add_peer_with_credentials_and_client_identity_and_label(
                    &card_url,
                    via_urls,
                    bearer_token,
                    pinned_fingerprints,
                    browser_tcp_via_url,
                    explicit_client_identity,
                    label,
                )
                .await
            {
                eprintln!(
                    "intendant: failed to register peer from intendant.toml \
                     ({card_url}): {e}"
                );
            }
        });
    }
    registry
}

fn peer_client_identity_from_config(
    cfg: &project::PeerConfig,
) -> Result<Option<peer::transport::tls_client::ClientIdentityPaths>, CallerError> {
    match (&cfg.client_cert, &cfg.client_key) {
        (Some(cert), Some(key)) => Ok(Some(peer::transport::tls_client::ClientIdentityPaths {
            cert_path: PathBuf::from(cert),
            key_path: PathBuf::from(key),
        })),
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err(CallerError::Config(format!(
            "[[peer]] card_url={} must set client_cert and client_key together",
            cfg.card_url
        ))),
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

const SAFETY_CAP: usize = 500;
const MIN_BUDGET_TOKENS: u64 = 4096;
const BUDGET_WARNING_THRESHOLD: f64 = 0.85;
const EXTERNAL_POST_TURN_DRAIN_GRACE: Duration = Duration::from_millis(750);

/// Why the agent loop exited after a round.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LoopExitReason {
    /// Agent sent an explicit done signal.
    DoneSignal,
    /// Task completed (no JSON, no commands, etc.).
    TaskComplete,
    /// Context budget exhausted.
    BudgetExhausted,
    /// Hit the safety cap of 500 turns.
    SafetyCapReached,
    /// User denied a command.
    Denied,
    /// An error occurred.
    #[allow(dead_code)]
    Error,
    /// User requested interruption mid-turn.
    Interrupted,
}

#[derive(Debug, Clone, Default)]
struct LoopStats {
    turns: usize,
    rounds: usize,
    terminal_outcome: Option<String>,
    usage: provider::TokenUsage,
    codex_subagent_sessions: std::collections::HashSet<String>,
    codex_subagent_parent_threads: std::collections::HashMap<String, String>,
    codex_subagent_rounds: std::collections::HashMap<String, usize>,
    codex_subagent_terminal_sessions: std::collections::HashSet<String>,
    codex_subagent_transcript_offsets: std::collections::HashMap<String, usize>,
    codex_subagent_tool_output_limiters:
        std::collections::HashMap<String, ExternalToolOutputLimiter>,
    codex_subagent_tool_failure_limiters:
        std::collections::HashMap<String, ExternalToolFailureLogLimiter>,
    /// Last model response content (for sub-agent result summaries).
    last_response: Option<String>,
    /// Native backend session id announced during the drained turn
    /// (`AgentEvent::NativeSessionId`). The CLI external-agent loop takes
    /// this after each drain to rotate its primary address, so targeted
    /// controls (thread actions, steer, stop) sent under the upgraded id
    /// keep matching this conversation.
    announced_native_session_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct UserAttachments {
    items: Vec<external_agent::AgentAttachment>,
}

impl UserAttachments {
    fn from_items(items: Vec<external_agent::AgentAttachment>) -> Self {
        Self { items }
    }

    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn len(&self) -> usize {
        self.items.len()
    }

    fn conversation_images(&self) -> Vec<conversation::ImageData> {
        self.items
            .iter()
            .filter_map(|att| match att {
                external_agent::AgentAttachment::Image(img) => Some(conversation::ImageData {
                    media_type: img.mime_type.clone(),
                    data: img.base64.clone(),
                }),
                external_agent::AgentAttachment::File(_) => None,
            })
            .collect()
    }

    fn text_with_file_prelude(&self, text: &str) -> String {
        let files: Vec<&external_agent::AgentFileAttachment> = self
            .items
            .iter()
            .filter_map(|att| match att {
                external_agent::AgentAttachment::File(file) => Some(file),
                external_agent::AgentAttachment::Image(_) => None,
            })
            .collect();
        let prelude = external_agent::format_file_attachments_prelude(&files);
        if prelude.is_empty() {
            text.to_string()
        } else {
            format!("{}{}", prelude, text)
        }
    }
}

#[derive(Debug, Clone, Default)]
struct FollowUpMessage {
    text: String,
    attachments: UserAttachments,
    steer_id: Option<String>,
    follow_up_id: Option<String>,
    edit_user_turn_index: Option<u32>,
    edit_user_turn_revision: Option<u32>,
    edit_original_text: Option<String>,
    unresolved_attachment_ids: Vec<String>,
    target_session_id: Option<String>,
    managed_context_recovery_kickstart: bool,
    managed_context_density_handoff: bool,
    managed_context_density_handoff_completed: bool,
}

impl FollowUpMessage {
    fn text(text: String) -> Self {
        Self {
            text,
            attachments: UserAttachments::default(),
            steer_id: None,
            follow_up_id: None,
            edit_user_turn_index: None,
            edit_user_turn_revision: None,
            edit_original_text: None,
            unresolved_attachment_ids: Vec::new(),
            target_session_id: None,
            managed_context_recovery_kickstart: false,
            managed_context_density_handoff: false,
            managed_context_density_handoff_completed: false,
        }
    }

    fn with_attachments(text: String, attachments: UserAttachments) -> Self {
        Self {
            text,
            attachments,
            steer_id: None,
            follow_up_id: None,
            edit_user_turn_index: None,
            edit_user_turn_revision: None,
            edit_original_text: None,
            unresolved_attachment_ids: Vec::new(),
            target_session_id: None,
            managed_context_recovery_kickstart: false,
            managed_context_density_handoff: false,
            managed_context_density_handoff_completed: false,
        }
    }

    fn steer(text: String, attachments: UserAttachments, steer_id: String) -> Self {
        Self {
            text,
            attachments,
            steer_id: Some(steer_id),
            follow_up_id: None,
            edit_user_turn_index: None,
            edit_user_turn_revision: None,
            edit_original_text: None,
            unresolved_attachment_ids: Vec::new(),
            target_session_id: None,
            managed_context_recovery_kickstart: false,
            managed_context_density_handoff: false,
            managed_context_density_handoff_completed: false,
        }
    }

    fn edit_user_message(
        text: String,
        attachments: UserAttachments,
        user_turn_index: u32,
        user_turn_revision: u32,
        original_text: Option<String>,
        unresolved_attachment_ids: Vec<String>,
    ) -> Self {
        Self {
            text,
            attachments,
            steer_id: None,
            follow_up_id: None,
            edit_user_turn_index: Some(user_turn_index),
            edit_user_turn_revision: Some(user_turn_revision),
            edit_original_text: original_text,
            unresolved_attachment_ids,
            target_session_id: None,
            managed_context_recovery_kickstart: false,
            managed_context_density_handoff: false,
            managed_context_density_handoff_completed: false,
        }
    }

    fn for_target(mut self, target_session_id: Option<String>) -> Self {
        self.target_session_id = target_session_id;
        self
    }

    fn with_follow_up_id(mut self, follow_up_id: Option<String>) -> Self {
        self.follow_up_id = follow_up_id;
        self
    }

    fn managed_context_recovery_kickstart(mut self) -> Self {
        self.managed_context_recovery_kickstart = true;
        self
    }

    fn managed_context_density_handoff(mut self) -> Self {
        self.managed_context_density_handoff = true;
        self
    }

    fn after_managed_context_density_handoff(mut self) -> Self {
        self.managed_context_density_handoff = false;
        self.managed_context_density_handoff_completed = true;
        self
    }
}

type FollowUpReceiver = tokio::sync::mpsc::Receiver<FollowUpMessage>;

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

/// Try binding to ports starting from `preferred`, returning the bound listener.
/// Avoids TOCTOU by keeping the listener alive instead of probing and releasing.
///
/// Binds dual-stack (IPv6 with `IPV6_V6ONLY=false`) so the listener
/// accepts both IPv6 and IPv4 connections. Without this, macOS
/// defaults `V6ONLY=true` on IPv6 sockets and an IPv4-only bind
/// would mismatch [`web_gateway::resolve_advertise_urls`], which
/// enumerates every routable interface (v4 and v6) into the Agent
/// Card. Federation code that picks a card URL verbatim — notably
/// slice 3b's `relay_advertise_url` — would then inject an
/// unreachable IPv6 ICE-TCP candidate and the browser would fail
/// to form a pair. Dual-stack keeps every advertised URL
/// truthful.
///
/// Falls back to IPv4-only if an IPv6 socket can't be created or
/// configured (containerized envs with no IPv6 stack, hardened
/// sandboxes that block V6ONLY toggling, etc). On those hosts
/// `routable_local_addrs` won't find any IPv6 interfaces either,
/// so the card's URL list stays consistent with the bind.
async fn find_available_port(
    preferred: u16,
    bind_ip: Option<IpAddr>,
) -> Result<(u16, tokio::net::TcpListener), CallerError> {
    for offset in 0..20u16 {
        let port = preferred.checked_add(offset).unwrap_or(preferred);
        match bind_web_listener(port, bind_ip).await {
            Ok(listener) => return Ok((port, listener)),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(e) => {
                return Err(CallerError::Config(format!(
                    "Failed to bind web gateway port: {}",
                    e
                )));
            }
        }
    }
    Err(CallerError::Config(format!(
        "No available port found in range {}-{}",
        preferred,
        preferred + 19
    )))
}

async fn bind_web_listener(
    port: u16,
    bind_ip: Option<IpAddr>,
) -> std::io::Result<tokio::net::TcpListener> {
    match bind_ip {
        None => bind_dual_stack_or_v4(port).await,
        Some(IpAddr::V6(ip)) if ip.is_unspecified() => bind_dual_stack_or_v4(port).await,
        Some(IpAddr::V4(ip)) => {
            bind_single_stack(SocketAddr::new(IpAddr::V4(ip), port), socket2::Domain::IPV4)
        }
        Some(IpAddr::V6(ip)) => {
            bind_single_stack(SocketAddr::new(IpAddr::V6(ip), port), socket2::Domain::IPV6)
        }
    }
}

fn bind_single_stack(
    addr: SocketAddr,
    domain: socket2::Domain,
) -> std::io::Result<tokio::net::TcpListener> {
    use socket2::{Protocol, Socket, Type};
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    let _ = socket.set_reuse_address(true);
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    let std_listener: std::net::TcpListener = socket.into();
    tokio::net::TcpListener::from_std(std_listener)
}

/// Bind a TCP listener on `port`, preferring IPv6 dual-stack.
/// See [`find_available_port`] for why dual-stack matters.
///
/// Uses `socket2` directly because `tokio::net::TcpSocket` doesn't
/// expose `IPV6_V6ONLY`. The constructed `std::net::TcpListener` is
/// set non-blocking and handed to tokio via `from_std`, which is the
/// same path tokio's own `TcpSocket::listen` takes under the hood.
///
/// Sets `SO_REUSEADDR` so a restart lands on the same port even
/// when the previous daemon's sockets are still in `TIME_WAIT`.
/// Without this, the Intendant.app wrapper's IPv4 probe (which
/// does set `SO_REUSEADDR`) says 8765 is free — the backend then
/// fails to bind it and slides to 8766, the WKWebView's HTTP poll
/// keeps hitting 8765, and the UI shows "Failed to connect to
/// backend on port 8765" even though the backend is healthy on
/// the next port. Matching the wrapper's assumption keeps the
/// port stable across restarts.
async fn bind_dual_stack_or_v4(port: u16) -> std::io::Result<tokio::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    if let Ok(socket) = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP)) {
        // Flip V6ONLY off so the listener accepts IPv4 too. If the
        // kernel doesn't support the toggle (hardened sandboxes),
        // fall through to the IPv4 fallback path.
        if socket.set_only_v6(false).is_ok() {
            // Best-effort: SO_REUSEADDR isn't load-bearing for
            // correctness (ignore Err), but without it a quick
            // restart races the kernel's TIME_WAIT window.
            let _ = socket.set_reuse_address(true);
            let v6_wildcard: SocketAddr = format!("[::]:{port}")
                .parse()
                .expect("IPv6 wildcard literal parses");
            // Propagate bind errors (AddrInUse / EACCES / etc) so the
            // caller's loop can walk to the next port or fail loudly.
            // Don't silently fall back to IPv4 here — an in-use IPv6
            // port is in use for IPv4 too on a dual-stack host.
            socket.bind(&v6_wildcard.into())?;
            socket.listen(1024)?;
            // tokio::net::TcpListener::from_std requires the underlying
            // socket to be in non-blocking mode.
            socket.set_nonblocking(true)?;
            let std_listener: std::net::TcpListener = socket.into();
            return tokio::net::TcpListener::from_std(std_listener);
        }
    }
    // IPv4 fallback for hosts without an IPv6 stack. Same TIME_WAIT
    // reasoning as the v6 path above — set SO_REUSEADDR via socket2
    // rather than going through tokio's bind (which doesn't expose it).
    let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    let _ = socket.set_reuse_address(true);
    let v4_wildcard: SocketAddr = format!("0.0.0.0:{port}")
        .parse()
        .expect("IPv4 wildcard literal parses");
    socket.bind(&v4_wildcard.into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    let std_listener: std::net::TcpListener = socket.into();
    tokio::net::TcpListener::from_std(std_listener)
}

/// Build the optional TLS acceptor for the `--web` dashboard.
///
/// The dashboard defaults to mTLS. `--tls` / `[server.tls] enabled = true`
/// explicitly select TLS-only, and `--no-tls` is the cleartext debug escape.
/// When TLS is enabled, the cert source is resolved in priority order:
///   1. Explicit PEM files — CLI `--tls-cert`/`--tls-key` first, else
///      `[server.tls] cert`/`key`. Both halves of a pair must be present.
///   2. Installed access certs (`server.crt` / `server.key`) from the platform's
///      `intendant access` cert directory.
///   3. For TLS-only, otherwise a self-signed cert minted by `rcgen`, with the
///      listener bind IP plus `localhost` (and optional `[server.tls] hostname`)
///      in the SAN list. mTLS never silently falls back to self-signed because
///      the browser also needs an enrolled client identity.
///
/// Returns `Ok(None)` only for `--no-tls`, `Ok(Some(acceptor))`
/// when on and the cert built, or `Err` when enabled but misconfigured
/// (mismatched cert/key pair, unreadable/invalid PEM, cert-gen failure) —
/// surfaced loudly at startup rather than silently serving plain HTTP.
fn build_web_tls_acceptor(
    flags: &CliFlags,
    server_cfg: &project::ServerTlsConfig,
    mtls_cfg: &project::ServerMutualTlsConfig,
    bind_addr: Option<std::net::SocketAddr>,
) -> Result<Option<tokio_rustls::TlsAcceptor>, CallerError> {
    if flags.no_tls {
        return Ok(None);
    }
    let mtls_enabled = web_mtls_enabled(flags, server_cfg, mtls_cfg);

    // Resolve an explicit cert/key pair: CLI overrides config. A
    // half-specified pair (only cert or only key) is a configuration
    // error rather than a silent fallback to self-signed.
    let cert_path = flags.tls_cert.clone().or_else(|| server_cfg.cert.clone());
    let key_path = flags.tls_key.clone().or_else(|| server_cfg.key.clone());
    let source = match (cert_path, key_path) {
        (Some(c), Some(k)) => web_tls::TlsCertSource::Files {
            cert_path: c.into(),
            key_path: k.into(),
        },
        (Some(_), None) | (None, Some(_)) => {
            return Err(CallerError::Config(
                "TLS cert/key must be supplied together (got only one of --tls-cert/--tls-key \
                 or [server.tls] cert/key)"
                    .to_string(),
            ));
        }
        (None, None) => match installed_access_tls_cert_source()? {
            Some(source) => source,
            None if mtls_enabled => {
                // A service-managed first boot has no human at a prompt: on
                // a machine whose access dir has never existed, provision
                // the same durable material `intendant access setup` would
                // create (CA + server pair + enrollable client identity)
                // and continue. Anything short of virgin still gets the
                // loud error — minting a new CA over one that browsers
                // already enrolled against would silently strand them.
                let provisioned = access::provision_virgin_access_certs().map_err(|e| {
                    CallerError::Config(format!(
                        "Dashboard mTLS is enabled by default and first-boot access \
                         certificate provisioning failed: {e}. Run `intendant access \
                         setup`, pass `--tls` for HTTPS without client certificate \
                         authentication, or pass `--no-tls --bind 127.0.0.1` only for \
                         explicit local/debug plaintext."
                    ))
                })?;
                match provisioned {
                    Some(cert_dir) => {
                        eprintln!(
                            "[access] first boot: generated dashboard access certificates \
                             in {} — enroll a browser via the claim flow or `intendant access`",
                            cert_dir.display()
                        );
                        installed_access_tls_cert_source()?.ok_or_else(|| {
                            CallerError::Config(missing_default_mtls_cert_message(
                                &installed_access_cert_dir(),
                            ))
                        })?
                    }
                    None => {
                        return Err(CallerError::Config(missing_default_mtls_cert_message(
                            &installed_access_cert_dir(),
                        )));
                    }
                }
            }
            None => web_tls::TlsCertSource::SelfSigned {
                bind_ip: bind_addr.map(|a| a.ip()),
                hostname: server_cfg.hostname.clone(),
            },
        },
    };

    let client_auth = if mtls_enabled {
        let ca_path = flags
            .mtls_ca
            .clone()
            .or_else(|| mtls_cfg.ca.clone())
            .map(PathBuf::from)
            .or_else(installed_access_mtls_ca_path);
        let Some(ca_path) = ca_path else {
            return Err(CallerError::Config(
                "mTLS requested, but no client CA was configured and no installed access CA \
                 was found. Run `intendant access setup` or pass --mtls-ca <ca.crt>."
                    .to_string(),
            ));
        };
        web_tls::ClientAuth::OptionalCa { ca_path }
    } else {
        web_tls::ClientAuth::None
    };

    match &source {
        web_tls::TlsCertSource::Files {
            cert_path,
            key_path,
        } => {
            eprintln!(
                "[web_gateway] TLS certificate source: {} / {}",
                cert_path.display(),
                key_path.display()
            );
        }
        web_tls::TlsCertSource::SelfSigned { .. } => {
            eprintln!("[web_gateway] TLS certificate source: ephemeral self-signed certificate");
        }
    }
    if let web_tls::ClientAuth::RequireCa { ca_path } = &client_auth {
        eprintln!("[web_gateway] mTLS client CA: {}", ca_path.display());
    }

    let acceptor = web_tls::build_acceptor_with_client_auth(&source, &client_auth)
        .map_err(|e| CallerError::Config(format!("TLS setup failed: {e}")))?;
    Ok(Some(acceptor))
}

fn web_mtls_enabled(
    flags: &CliFlags,
    server_cfg: &project::ServerTlsConfig,
    mtls_cfg: &project::ServerMutualTlsConfig,
) -> bool {
    if flags.no_tls {
        return false;
    }
    flags.mtls || mtls_cfg.enabled || web_default_mtls_enabled(flags, server_cfg)
}

fn web_default_mtls_enabled(flags: &CliFlags, server_cfg: &project::ServerTlsConfig) -> bool {
    !flags.no_tls
        && !flags.tls
        && !server_cfg.enabled
        && flags.tls_cert.is_none()
        && flags.tls_key.is_none()
}

fn missing_default_mtls_cert_message(cert_dir: &Path) -> String {
    format!(
        "Dashboard mTLS is enabled by default, but no installed access server certificate was \
         found in {cert_dir} (expected server.crt and server.key). The directory holds other \
         access material, so first-boot auto-provisioning stayed hands-off rather than touch an \
         existing CA. Run `intendant access setup` to (re)generate what's missing, pass `--tls` \
         for HTTPS without client certificate authentication, or pass `--no-tls --bind \
         127.0.0.1` only for explicit local/debug plaintext.",
        cert_dir = cert_dir.display()
    )
}

fn installed_access_cert_dir() -> PathBuf {
    access::backend::select_backend().cert_dir()
}

fn installed_access_tls_cert_source() -> Result<Option<web_tls::TlsCertSource>, CallerError> {
    let cert_dir = installed_access_cert_dir();
    installed_access_tls_cert_source_from_dir(&cert_dir)
}

fn installed_access_tls_cert_source_from_dir(
    cert_dir: &Path,
) -> Result<Option<web_tls::TlsCertSource>, CallerError> {
    installed_access_tls_cert_source_from_dir_with_probe(cert_dir, |path| {
        std::fs::File::open(path).map(|_| ())
    })
}

fn installed_access_tls_cert_source_from_dir_with_probe(
    cert_dir: &Path,
    can_read: impl Fn(&Path) -> io::Result<()>,
) -> Result<Option<web_tls::TlsCertSource>, CallerError> {
    let cert_path = cert_dir.join("server.crt");
    let key_path = cert_dir.join("server.key");
    let cert_exists = cert_path.exists();
    let key_exists = key_path.exists();
    match (cert_exists, key_exists) {
        (true, true) => {
            ensure_installed_access_tls_file_readable(
                cert_dir,
                &cert_path,
                "certificate",
                &can_read,
            )?;
            ensure_installed_access_tls_file_readable(
                cert_dir,
                &key_path,
                "private key",
                &can_read,
            )?;
            Ok(Some(web_tls::TlsCertSource::Files {
                cert_path,
                key_path,
            }))
        }
        (false, false) => Ok(None),
        _ => Err(CallerError::Config(format!(
            "Installed access TLS certs are incomplete in {} (expected both server.crt and \
             server.key). Run `intendant access setup --force` or pass --tls-cert/--tls-key.",
            cert_dir.display()
        ))),
    }
}

fn ensure_installed_access_tls_file_readable(
    cert_dir: &Path,
    path: &Path,
    role: &str,
    can_read: &impl Fn(&Path) -> io::Result<()>,
) -> Result<(), CallerError> {
    can_read(path).map_err(|err| {
        CallerError::Config(installed_access_tls_unreadable_message(
            cert_dir, path, role, &err,
        ))
    })
}

fn installed_access_tls_unreadable_message(
    cert_dir: &Path,
    path: &Path,
    role: &str,
    err: &io::Error,
) -> String {
    let permission_hint = if err.kind() == io::ErrorKind::PermissionDenied {
        String::from(
            " To let this user run native `--tls` with the installed access cert, \
             fix ownership of the per-user access cert store or rerun \
             `intendant access setup --force` as that user.",
        )
    } else {
        String::new()
    };
    format!(
        "Installed access TLS {role} exists at {path}, but this process cannot read it: {err}. \
         Native `--tls` reads the server certificate and key directly from the per-user \
         access cert store at {cert_dir}.{permission_hint} Alternatively, pass a readable pair with \
         `--tls-cert <cert> --tls-key <key>`, or move the installed pair out of {cert_dir} to use \
         the self-signed fallback.",
        path = path.display(),
        cert_dir = cert_dir.display(),
    )
}

fn installed_access_mtls_ca_path() -> Option<PathBuf> {
    installed_access_mtls_ca_path_from_dir(&installed_access_cert_dir())
}

fn installed_access_mtls_ca_path_from_dir(cert_dir: &Path) -> Option<PathBuf> {
    let ca_path = cert_dir.join("ca.crt");
    ca_path.exists().then_some(ca_path)
}

fn web_tui_display_url(
    web_tls_acceptor: &Option<tokio_rustls::TlsAcceptor>,
    web_port: u16,
    web_bind: Option<IpAddr>,
) -> String {
    let scheme = if web_tls_acceptor.is_some() {
        "https"
    } else {
        "http"
    };
    let host = web_tui_display_host(web_bind);
    format!("{scheme}://{host}:{web_port}")
}

fn web_tui_display_host(web_bind: Option<IpAddr>) -> String {
    match web_bind {
        Some(IpAddr::V4(ip)) => ip.to_string(),
        Some(IpAddr::V6(ip)) => format!("[{ip}]"),
        None => "0.0.0.0".to_string(),
    }
}

fn web_tui_log_line(
    web_tls_acceptor: &Option<tokio_rustls::TlsAcceptor>,
    web_port: u16,
    web_bind: Option<IpAddr>,
) -> String {
    format!(
        "Web TUI: {}",
        web_tui_display_url(web_tls_acceptor, web_port, web_bind)
    )
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

fn validate_tls_cli_flags(flags: &CliFlags) -> Result<(), CallerError> {
    if flags.no_tls
        && (flags.tls
            || flags.mtls
            || flags.tls_cert.is_some()
            || flags.tls_key.is_some()
            || flags.mtls_ca.is_some())
    {
        return Err(CallerError::Config(
            "`--no-tls` cannot be combined with `--tls`, `--mtls`, `--tls-cert`, \
             `--tls-key`, or `--mtls-ca`."
                .to_string(),
        ));
    }
    Ok(())
}

fn effective_web_bind_ip(flags: &CliFlags, server_cfg: &project::ServerConfig) -> Option<IpAddr> {
    flags.web_bind.or(server_cfg.bind)
}

fn validate_plaintext_web_bind(
    flags: &CliFlags,
    bind_ip: Option<IpAddr>,
) -> Result<(), CallerError> {
    let public_addrs = public_routable_local_addrs();
    validate_plaintext_web_bind_with_public_addrs(flags, bind_ip, &public_addrs)
}

fn validate_plaintext_web_bind_with_public_addrs(
    flags: &CliFlags,
    bind_ip: Option<IpAddr>,
    public_addrs: &[IpAddr],
) -> Result<(), CallerError> {
    if !flags.no_tls
        || flags.allow_public_plaintext
        || !web_bind_is_wildcard(bind_ip)
        || public_addrs.is_empty()
    {
        return Ok(());
    }

    let public_list = public_addrs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    Err(CallerError::Config(format!(
        "Refusing `--no-tls` on a wildcard dashboard listener because this host has public \
         interface address(es): {public_list}. Plain HTTP would expose Intendant on those \
         addresses. Use default mTLS, `--tls`, `--bind 127.0.0.1`, bind a specific private \
         interface, or pass `--allow-public-plaintext` if this is intentional."
    )))
}

fn web_bind_is_wildcard(bind_ip: Option<IpAddr>) -> bool {
    bind_ip.map(|ip| ip.is_unspecified()).unwrap_or(true)
}

fn public_routable_local_addrs() -> Vec<IpAddr> {
    let mut addrs = access::routable_local_addrs(false)
        .into_iter()
        .filter(is_public_ip)
        .collect::<Vec<_>>();
    addrs.sort_by_key(|ip| ip.to_string());
    addrs.dedup();
    addrs
}

fn is_public_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_ipv4(*ip),
        IpAddr::V6(ip) => is_public_ipv6(*ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_private()
        && !ip.is_link_local()
        && !ip.is_multicast()
        && !ip.is_broadcast()
        && !ip.is_documentation()
        && !is_shared_carrier_nat_ipv4(ip)
}

fn is_shared_carrier_nat_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (octets[1] & 0b1100_0000) == 64
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();
    let first = segments[0];
    let is_unique_local = (first & 0xfe00) == 0xfc00;
    let is_link_local = (first & 0xffc0) == 0xfe80;
    let is_documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;
    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_multicast()
        && !is_unique_local
        && !is_link_local
        && !is_documentation
}

fn should_start_idle_web_daemon(use_web: bool, flags: &CliFlags) -> bool {
    use_web
        && !flags.mcp
        && flags.task_file.is_none()
        && flags
            .task
            .as_ref()
            .map(|task| task.trim().is_empty())
            .unwrap_or(true)
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

    fn access_names(ip: &str) -> access::certs::ServerNames {
        access::certs::ServerNames::new(
            ip.parse().unwrap(),
            Vec::<std::net::IpAddr>::new(),
            Vec::<String>::new(),
        )
        .unwrap()
    }

    fn peer_config_with_client_identity(
        client_cert: Option<&str>,
        client_key: Option<&str>,
    ) -> project::PeerConfig {
        project::PeerConfig {
            card_url: "https://peer.example/.well-known/agent-card.json".to_string(),
            label: None,
            bearer_token: None,
            via_urls: Vec::new(),
            client_cert: client_cert.map(str::to_string),
            client_key: client_key.map(str::to_string),
            pinned_fingerprints: Vec::new(),
            browser_tcp_via_url: None,
        }
    }

    #[test]
    fn peer_client_identity_config_requires_cert_and_key() {
        let cfg =
            peer_config_with_client_identity(Some("/tmp/client.crt"), Some("/tmp/client.key"));
        let identity = peer_client_identity_from_config(&cfg).unwrap().unwrap();
        assert_eq!(identity.cert_path, PathBuf::from("/tmp/client.crt"));
        assert_eq!(identity.key_path, PathBuf::from("/tmp/client.key"));

        assert!(
            peer_client_identity_from_config(&peer_config_with_client_identity(None, None))
                .unwrap()
                .is_none()
        );
        let err =
            peer_client_identity_from_config(&peer_config_with_client_identity(Some("x"), None))
                .unwrap_err()
                .to_string();
        assert!(err.contains("client_cert and client_key together"));
    }

    /// `build_local_advertised_auth` with the default config (all
    /// `[server.auth]` fields unset) produces `AuthRequirements::none()`
    /// — the conservative default that doesn't advertise any auth.
    /// Doesn't touch the cert dir at all; safe to run with no access setup.
    #[test]
    fn build_local_advertised_auth_defaults_to_none() {
        let server_auth = project::ServerAuthConfig::default();
        let cert_dir = std::path::PathBuf::from("/nonexistent");
        let auth = build_local_advertised_auth(&server_auth, &cert_dir).unwrap();
        assert_eq!(auth, peer::AuthRequirements::none());
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

    /// Catalog entry with surgical-test defaults; tests override the fields
    /// the chooser actually reads (lines, ordinal, eligibility, names).
    /// Regression test for the live 2026-06-11 context-stress failure: codex
    /// persists a tool's `function_call_output` *before* the `token_count` of
    /// the response that emitted the call, so that report never measured the
    /// output. Attributing it to the call/output group made `after` (which
    /// keeps the bulky output) look recovery-eligible and suppressed `before`
    /// (the only cut that actually recovers).
    /// Idempotence across listing-only growth: a recovery stall appends only
    /// management calls (listings, status polls), and those must not change
    /// the model-visible catalog accounting between two identical listings.
    /// The type-B dead-end from the 2026-06-12 bench: a thread whose only
    /// remaining items are management/status calls must say plainly that
    /// nothing is left to rewind to instead of returning a bare empty page.
    /// `advertised_transport = "mutual-tls"` advertises plain mTLS.
    /// Doesn't read the cert dir (no fingerprint to compute).
    #[test]
    fn build_local_advertised_auth_mutual_tls_no_cert_lookup() {
        let server_auth = project::ServerAuthConfig {
            bearer_token: None,
            advertised_transport: "mutual-tls".to_string(),
        };
        let cert_dir = std::path::PathBuf::from("/nonexistent");
        let auth = build_local_advertised_auth(&server_auth, &cert_dir).unwrap();
        assert!(matches!(auth.transport, peer::TransportAuth::MutualTls));
        assert!(auth.application.is_none());
    }

    /// `advertised_transport = "pin-self-cert"` reads the access cert
    /// dir, computes the fingerprint, embeds it in PinnedMutualTls.
    /// Uses `access::certs::ensure_certs` to populate a tempdir.
    /// `access::certs` is now pure-Rust and compiles everywhere, so this
    /// applies on all platforms.
    #[test]
    fn build_local_advertised_auth_pin_self_cert_reads_cert_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        access::certs::ensure_certs(tmp.path(), &access_names("10.0.0.1"), "test", false).unwrap();
        let expected_fp = access::certs::read_server_cert_fingerprint(tmp.path()).unwrap();

        let server_auth = project::ServerAuthConfig {
            bearer_token: None,
            advertised_transport: "pin-self-cert".to_string(),
        };
        let auth = build_local_advertised_auth(&server_auth, tmp.path()).unwrap();
        match &auth.transport {
            peer::TransportAuth::PinnedMutualTls {
                server_cert_fingerprints,
            } => {
                assert_eq!(server_cert_fingerprints, &vec![expected_fp]);
            }
            other => panic!("expected PinnedMutualTls, got {other:?}"),
        }
    }

    /// `advertised_transport = "pin-self-cert"` with no cert in
    /// the dir errors with a clear message that points the
    /// operator at `intendant access setup`.
    #[test]
    fn build_local_advertised_auth_pin_self_cert_errors_without_cert() {
        let tmp = tempfile::TempDir::new().unwrap();
        let server_auth = project::ServerAuthConfig {
            bearer_token: None,
            advertised_transport: "pin-self-cert".to_string(),
        };
        let err = build_local_advertised_auth(&server_auth, tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("server.crt"), "msg: {msg}");
        assert!(msg.contains("intendant access setup"), "msg: {msg}");
    }

    /// Unrecognized `advertised_transport` value errors loudly at
    /// startup so the operator notices the typo (vs. silent fall
    /// back to "none" which would surprise them).
    #[test]
    fn build_local_advertised_auth_rejects_invalid_transport_value() {
        let server_auth = project::ServerAuthConfig {
            bearer_token: None,
            advertised_transport: "definitely-not-valid".to_string(),
        };
        let cert_dir = std::path::PathBuf::from("/nonexistent");
        let err = build_local_advertised_auth(&server_auth, &cert_dir).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("definitely-not-valid"), "msg: {msg}");
        assert!(msg.contains("none"), "msg: {msg}");
        assert!(msg.contains("mutual-tls"), "msg: {msg}");
        assert!(msg.contains("pin-self-cert"), "msg: {msg}");
    }

    /// `bearer_token` set produces `application = Some(Bearer)`
    /// regardless of the transport value. The `hint` field
    /// documents where the token comes from so connecting peers
    /// can give operators a useful "configure me" message.
    #[test]
    fn build_local_advertised_auth_bearer_token_sets_application() {
        let server_auth = project::ServerAuthConfig {
            bearer_token: Some("secret".to_string()),
            advertised_transport: "none".to_string(),
        };
        let cert_dir = std::path::PathBuf::from("/nonexistent");
        let auth = build_local_advertised_auth(&server_auth, &cert_dir).unwrap();
        match &auth.application {
            Some(peer::ApplicationAuth::Bearer { hint, rotation_url }) => {
                assert!(hint.is_some(), "hint should document the source");
                assert!(hint.as_ref().unwrap().contains("[server.auth]"));
                assert!(
                    rotation_url.is_none(),
                    "rotation_url unset until rotation lands"
                );
            }
            other => panic!("expected Bearer application auth, got {other:?}"),
        }
    }

    /// Combination: `pin-self-cert` + `bearer_token` produces the
    /// full defense-in-depth advertise (PinnedMutualTls transport +
    /// Bearer application). The expected configuration for WAN-
    /// exposed daemons that want both wire-layer and app-layer auth.
    /// `access::certs` is now pure-Rust and compiles everywhere, so this
    /// applies on all platforms.
    #[test]
    fn build_local_advertised_auth_full_defense_in_depth() {
        let tmp = tempfile::TempDir::new().unwrap();
        access::certs::ensure_certs(tmp.path(), &access_names("10.0.0.99"), "wan-test", false)
            .unwrap();

        let server_auth = project::ServerAuthConfig {
            bearer_token: Some("wan-secret".to_string()),
            advertised_transport: "pin-self-cert".to_string(),
        };
        let auth = build_local_advertised_auth(&server_auth, tmp.path()).unwrap();
        assert!(matches!(
            auth.transport,
            peer::TransportAuth::PinnedMutualTls { .. }
        ));
        assert!(matches!(
            auth.application,
            Some(peer::ApplicationAuth::Bearer { .. })
        ));
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

    #[test]
    fn public_ip_classification_excludes_private_and_documentation_ranges() {
        assert!(is_public_ip(&"8.8.8.8".parse().unwrap()));
        assert!(!is_public_ip(&"10.0.0.1".parse().unwrap()));
        assert!(!is_public_ip(&"192.168.1.10".parse().unwrap()));
        assert!(!is_public_ip(&"100.64.0.1".parse().unwrap()));
        assert!(!is_public_ip(&"203.0.113.10".parse().unwrap()));
        assert!(is_public_ip(&"2001:4860:4860::8888".parse().unwrap()));
        assert!(!is_public_ip(&"fc00::1".parse().unwrap()));
        assert!(!is_public_ip(&"fe80::1".parse().unwrap()));
        assert!(!is_public_ip(&"2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn installed_access_tls_source_uses_complete_server_pair() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("server.crt");
        let key_path = dir.path().join("server.key");
        std::fs::write(&cert_path, b"cert").unwrap();
        std::fs::write(&key_path, b"key").unwrap();

        let source = installed_access_tls_cert_source_from_dir(dir.path())
            .unwrap()
            .expect("access cert pair should be discovered");
        match source {
            web_tls::TlsCertSource::Files {
                cert_path: c,
                key_path: k,
            } => {
                assert_eq!(c, cert_path);
                assert_eq!(k, key_path);
            }
            other => panic!("expected file source, got {other:?}"),
        }
    }

    #[test]
    fn installed_access_tls_source_ignores_absent_pair() {
        let dir = tempfile::tempdir().unwrap();
        assert!(installed_access_tls_cert_source_from_dir(dir.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn installed_access_tls_source_errors_on_partial_pair() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.crt"), b"cert").unwrap();
        let err = installed_access_tls_cert_source_from_dir(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("incomplete"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn installed_access_tls_source_explains_unreadable_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.crt"), b"cert").unwrap();
        std::fs::write(dir.path().join("server.key"), b"key").unwrap();

        let err = installed_access_tls_cert_source_from_dir_with_probe(dir.path(), |path| {
            if path.ends_with("server.key") {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "permission denied",
                ))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cannot read it"), "msg: {msg}");
        assert!(msg.contains("server.key"), "msg: {msg}");
        assert!(msg.contains("per-user access cert store"), "msg: {msg}");
        assert!(msg.contains("intendant access setup --force"), "msg: {msg}");
        assert!(
            msg.contains("--tls-cert <cert> --tls-key <key>"),
            "msg: {msg}"
        );
    }

    #[test]
    fn installed_access_mtls_ca_uses_ca_crt_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let ca_path = dir.path().join("ca.crt");
        std::fs::write(&ca_path, b"ca").unwrap();
        assert_eq!(
            installed_access_mtls_ca_path_from_dir(dir.path()).as_deref(),
            Some(ca_path.as_path())
        );
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

fn orchestration_unavailable() -> String {
    "Error: sub-agent orchestration is only available in supervised sessions under the \
     web daemon (the default mode). This session has no session supervisor, so \
     spawn_sub_agent / wait_sub_agents cannot run here."
        .to_string()
}

/// Handle a spawn_sub_agent tool call: spawn a supervised child session
/// through the session supervisor and track it on this session's
/// orchestration handle for wait_sub_agents.
async fn handle_spawn_sub_agent_call(
    args: &serde_json::Value,
    orchestration: Option<&session_supervisor::SessionOrchestration>,
    project: &Project,
    session_log: &SharedSessionLog,
) -> String {
    let Some(orchestration) = orchestration else {
        return orchestration_unavailable();
    };
    let task = args
        .get("task")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if task.is_empty() {
        return "Error: spawn_sub_agent requires a non-empty `task`.".to_string();
    }
    let role = sub_agent::SubAgentRole::from_str(
        args.get("role")
            .and_then(|r| r.as_str())
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .unwrap_or("worker"),
    );
    let backend = match args
        .get("backend")
        .and_then(|b| b.as_str())
        .map(str::trim)
        .unwrap_or("internal")
    {
        "internal" | "" => None,
        "codex" => Some(external_agent::AgentBackend::Codex),
        "claude-code" | "claude_code" => Some(external_agent::AgentBackend::ClaudeCode),
        other => {
            return format!(
                "Error: unknown sub-agent backend `{other}`; use internal, codex, or claude-code."
            );
        }
    };
    let params = session_supervisor::SubAgentSpawnParams {
        task,
        role,
        system_prompt: args
            .get("system_prompt")
            .and_then(|p| p.as_str())
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(String::from),
        backend,
        worktree: args
            .get("worktree")
            .and_then(|w| w.as_bool())
            .unwrap_or(false),
        inherit_memory: args
            .get("inherit_memory")
            .and_then(|i| i.as_bool())
            .unwrap_or(false),
        name: args
            .get("name")
            .and_then(|n| n.as_str())
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(String::from),
    };
    match orchestration
        .supervisor
        .start_sub_agent_session(&orchestration.session_id, project, params)
        .await
    {
        Ok(started) => {
            slog(session_log, |l| {
                l.info(&format!(
                    "Spawned sub-agent {} (session {})",
                    started.child_name,
                    session_supervisor::short_session(&started.child_session_id)
                ))
            });
            let mut response = format!(
                "Sub-agent spawned.\n- name: {}\n- child_session_id: {}",
                started.child_name, started.child_session_id
            );
            if let Some(path) = &started.worktree_path {
                response.push_str(&format!("\n- worktree: {}", path.display()));
            }
            response.push_str(
                "\nIt is running as its own supervised session. Collect its result with wait_sub_agents.",
            );
            let mut children = orchestration
                .children
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            children.insert(
                started.child_session_id.clone(),
                session_supervisor::SubAgentChild {
                    name: started.child_name,
                    rx: Some(started.completion_rx),
                    completed: None,
                    delivered: false,
                },
            );
            response
        }
        Err(e) => format!("Error: {e}"),
    }
}

/// Handle a submit_result tool call from a sub-agent child: record the
/// structured result in the slot the supervisor delivers to the parent
/// when this session finishes.
fn handle_submit_result_call(
    args: &serde_json::Value,
    orchestration: Option<&session_supervisor::SessionOrchestration>,
    local_session_id: &Option<String>,
) -> String {
    let Some(slot) = orchestration.and_then(|o| o.submitted_result.as_ref()) else {
        return "Error: submit_result is only available to sessions spawned as sub-agents."
            .to_string();
    };
    let summary = args
        .get("summary")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if summary.is_empty() {
        return "Error: submit_result requires a non-empty `summary`.".to_string();
    }
    let status = match args
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("completed")
    {
        "completed" => sub_agent::SubAgentStatus::Completed,
        "failed" => sub_agent::SubAgentStatus::Failed(
            args.get("failure_reason")
                .and_then(|r| r.as_str())
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .unwrap_or("unspecified failure")
                .to_string(),
        ),
        other => {
            return format!("Error: unknown status `{other}`; use `completed` or `failed`.");
        }
    };
    let brief = args
        .get("brief")
        .and_then(|b| b.as_str())
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(String::from)
        .unwrap_or_else(|| parse_brief(&summary).0);
    let findings = args
        .get("findings")
        .and_then(|f| f.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let artifacts = args
        .get("artifacts")
        .and_then(|f| f.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(PathBuf::from))
                .collect()
        })
        .unwrap_or_default();
    let result = sub_agent::SubAgentResult {
        id: local_session_id.clone().unwrap_or_default(),
        status,
        summary,
        brief,
        findings,
        artifacts,
        // Usage comes from session accounting, not self-report.
        usage: provider::TokenUsage::default(),
    };
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
    "Result recorded. It is delivered to your parent session when you finish — call signal_done once your work is complete."
        .to_string()
}

/// Handle a wait_sub_agents tool call: block until the requested children
/// finish (mode `all`, default) or the first one does (mode `any`), the
/// timeout lapses, or the user interrupts/stops this session.
async fn handle_wait_sub_agents_call(
    args: &serde_json::Value,
    orchestration: Option<&session_supervisor::SessionOrchestration>,
    bus: &EventBus,
    local_session_id: &Option<String>,
    session_log: &SharedSessionLog,
) -> String {
    let Some(orchestration) = orchestration else {
        return orchestration_unavailable();
    };
    let wait_all = !matches!(args.get("mode").and_then(|m| m.as_str()), Some("any"));
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|t| t.as_u64())
        .unwrap_or(600)
        .clamp(5, 7200);
    let filter: Option<std::collections::HashSet<String>> = args
        .get("agent_ids")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .filter(|set: &std::collections::HashSet<String>| !set.is_empty());

    let target_ids: Vec<String> = {
        let children = orchestration
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        children
            .iter()
            .filter(|(id, child)| {
                !child.delivered
                    && filter
                        .as_ref()
                        .map(|f| f.contains(*id) || f.contains(&child.name))
                        .unwrap_or(true)
            })
            .map(|(id, _)| id.clone())
            .collect()
    };
    if target_ids.is_empty() {
        return "No pending sub-agents to wait for: every spawned sub-agent's result was \
                already delivered (or none match the requested agent_ids)."
            .to_string();
    }

    slog(session_log, |l| {
        l.info(&format!(
            "Waiting for {} sub-agent(s) (mode: {}, timeout: {}s)",
            target_ids.len(),
            if wait_all { "all" } else { "any" },
            timeout_secs
        ))
    });

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let mut interrupt_rx = bus.subscribe();
    let mut interrupted = false;
    let mut timed_out = false;

    loop {
        let satisfied = {
            let mut children = orchestration
                .children
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut ready = 0usize;
            for id in &target_ids {
                let Some(child) = children.get_mut(id) else {
                    ready += 1; // vanished child counts as resolved
                    continue;
                };
                if child.completed.is_none() && !child.delivered {
                    if let Some(rx) = child.rx.as_mut() {
                        match rx.try_recv() {
                            Ok(completion) => {
                                child.completed = Some(completion);
                                child.rx = None;
                            }
                            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                                child.rx = None;
                                child.completed =
                                    Some(session_supervisor::SubAgentCompletion {
                                        child_session_id: id.clone(),
                                        name: child.name.clone(),
                                        result: sub_agent::SubAgentResult {
                                            id: child.name.clone(),
                                            status: sub_agent::SubAgentStatus::Failed(
                                                "session ended without a result".to_string(),
                                            ),
                                            summary:
                                                "Sub-agent session ended without reporting a result"
                                                    .to_string(),
                                            brief: "Sub-agent ended without a result.".to_string(),
                                            findings: vec![],
                                            artifacts: vec![],
                                            usage: provider::TokenUsage::default(),
                                        },
                                    });
                            }
                        }
                    }
                }
                if child.completed.is_some() || child.delivered {
                    ready += 1;
                }
            }
            if wait_all {
                ready >= target_ids.len()
            } else {
                ready > 0
            }
        };
        if satisfied {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        while let Ok(event) = interrupt_rx.try_recv() {
            match event {
                AppEvent::InterruptRequested { session_id }
                | AppEvent::SessionStopRequested { session_id, .. }
                    if event_targets_session(&session_id, local_session_id) =>
                {
                    interrupted = true;
                }
                _ => {}
            }
        }
        if interrupted {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    let mut delivered = Vec::new();
    let mut still_running = Vec::new();
    {
        let mut children = orchestration
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for id in &target_ids {
            let Some(child) = children.get_mut(id) else {
                continue;
            };
            if child.delivered {
                continue;
            }
            match child.completed.as_ref() {
                Some(completion) => {
                    child.delivered = true;
                    delivered.push(format!(
                        "{} (session {})\n{}",
                        completion.name,
                        completion.child_session_id,
                        sub_agent::format_result_message(&completion.result)
                    ));
                }
                None => still_running.push(format!("{} ({})", child.name, id)),
            }
        }
    }

    let mut out = String::new();
    if interrupted {
        out.push_str("[wait interrupted by the user]\n\n");
    } else if timed_out && delivered.is_empty() {
        out.push_str(&format!(
            "[wait timed out after {timeout_secs}s with no completions]\n\n"
        ));
    }
    if !delivered.is_empty() {
        out.push_str(&delivered.join("\n\n"));
    }
    if !still_running.is_empty() {
        out.push_str(&format!(
            "\n\nStill running: {}. Call wait_sub_agents again to keep waiting, or proceed and collect them later.",
            still_running.join(", ")
        ));
    }
    if delivered.is_empty() && still_running.is_empty() {
        out.push_str("All requested sub-agents had already delivered their results.");
    }
    out.trim().to_string()
}

#[allow(clippy::too_many_arguments)]
async fn run_agent_loop(
    provider: &dyn provider::ChatProvider,
    conversation: &mut Conversation,
    project: &Project,
    sub_agent_mode: Option<&(String, sub_agent::SubAgentRole)>,
    bus: &EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: &std::path::Path,
    mcp_mgr: Option<&mcp_client::McpClientManager>,
    json_approval: Option<&JsonApprovalSlot>,
    approval_registry: &event::ApprovalRegistry,
    context_injection: &event::ContextInjectionQueue,
    xvfb_guard: &mut Option<vision::XvfbGuard>,
    session_registry: Option<&display::SharedSessionRegistry>,
    // When true, askHuman is unavailable and approvals without a json_approval
    // slot are auto-denied (headless non-JSON mode).
    headless: bool,
    // Supervised-session orchestration handle: enables the
    // spawn_sub_agent / wait_sub_agents / submit_result tools. None outside
    // the daemon, where those tools answer with a clear error.
    orchestration: Option<&session_supervisor::SessionOrchestration>,
) -> Result<(LoopStats, LoopExitReason), CallerError> {
    let mut budget_warning_shown = false;
    let mut empty_command_streak = 0usize;
    let mut cu_action_counter = 0u64;
    let mut loop_stats = LoopStats::default();
    let mut exit_reason = LoopExitReason::TaskComplete;

    // Discard stale System injections from before this task started
    // (e.g. display take/release events that happened while idle), but
    // PRESERVE User injections — those come from the dashboard's annotation
    // Send button and may have been queued while the agent was idle. We owe
    // the user the courtesy of actually delivering what they sent.
    if let Ok(mut q) = context_injection.lock() {
        q.retain(|inj| inj.source == event::InjectionSource::User);
    }

    // Cancellation plumbing: a watcher task flips the token when it sees
    // AppEvent::InterruptRequested on the bus, and drains the approval
    // registry so any in-flight `rx.await` inside the approval handler
    // unblocks immediately. The loop checks the token at its boundaries
    // and wraps the streaming API call in tokio::select! so an interrupt
    // mid-stream drops the response cleanly.
    //
    // The same watcher also handles AppEvent::SteerRequested: it pushes
    // the steer text onto the shared `context_injection` queue (tagged as
    // a user injection so it survives inter-task drains) and emits
    // `SteerAccepted`. The native agent loop drains `context_injection` at
    // the top of every turn and emits `SteerDelivered` at that point, so
    // queued steers are distinguishable from actual model-context delivery.
    // We keep the watcher alive across multiple steers — unlike the interrupt
    // branch which exits after cancelling.
    let local_session_id = session_log_id(&session_log);
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let cancel_watcher_handle = {
        let watcher_token = cancel_token.clone();
        let watcher_registry = approval_registry.clone();
        let watcher_injection = context_injection.clone();
        let watcher_bus = bus.clone();
        let watcher_session_id = local_session_id.clone();
        let mut bus_rx = bus.subscribe();
        tokio::spawn(async move {
            loop {
                match bus_rx.recv().await {
                    Ok(AppEvent::InterruptRequested { session_id })
                        if event_targets_session(&session_id, &watcher_session_id) =>
                    {
                        // Drain pending approvals with Deny so their
                        // receivers unblock and the loop can reach its
                        // cancellation-check boundary.
                        let pending: Vec<_> = {
                            let mut reg = watcher_registry.lock().unwrap();
                            reg.drain().collect()
                        };
                        for (_, sender) in pending {
                            let _ = sender.send(event::ApprovalResponse::Deny);
                        }
                        watcher_token.cancel();
                        break;
                    }
                    Ok(AppEvent::SteerRequested {
                        session_id,
                        text,
                        id,
                    }) if event_targets_session(&session_id, &watcher_session_id) => {
                        // Queue the steer for the next turn's drain. The
                        // native loop has no separate "mid-turn inject"
                        // hook — model calls are atomic — so acceptance and
                        // delivery are separate UI states.
                        if let Ok(mut q) = watcher_injection.lock() {
                            q.push(event::ContextInjection::text_with_steer_id_for_target(
                                text,
                                id.clone(),
                                watcher_session_id.clone(),
                            ));
                        }
                        watcher_bus.send(AppEvent::SteerAccepted {
                            session_id: watcher_session_id.clone(),
                            id,
                            reason: "Queued for the next model checkpoint".to_string(),
                        });
                    }
                    Ok(AppEvent::SteerCancelRequested {
                        session_id,
                        id,
                        reason,
                    }) if event_targets_session(&session_id, &watcher_session_id) => {
                        let removed = cancel_queued_steers_for_session(
                            &watcher_injection,
                            &watcher_bus,
                            watcher_session_id.as_deref(),
                            None,
                            id.as_deref(),
                            &reason,
                        );
                        if removed == 0 {
                            if let Some(id) = id.filter(|id| !id.trim().is_empty()) {
                                watcher_bus.send(AppEvent::SteerCancelled {
                                    session_id: watcher_session_id.clone(),
                                    id,
                                    reason,
                                });
                            }
                        }
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };

    // Guard that aborts the watcher and drains approvals exactly once on
    // any exit (interrupt OR normal completion). We cancel the watcher on
    // drop so it stops listening, and we proactively resolve any pending
    // approvals with Deny if the exit path was interrupt-driven.
    struct InterruptGuard {
        watcher: Option<tokio::task::JoinHandle<()>>,
    }
    impl Drop for InterruptGuard {
        fn drop(&mut self) {
            if let Some(h) = self.watcher.take() {
                h.abort();
            }
        }
    }
    let _guard = InterruptGuard {
        watcher: Some(cancel_watcher_handle),
    };

    for turn in 1..=SAFETY_CAP {
        // Interrupt check at loop boundary.
        if cancel_token.is_cancelled() {
            // Drain and deny any pending approvals so their receivers unblock.
            let pending: Vec<_> = {
                let mut reg = approval_registry.lock().unwrap();
                reg.drain().collect()
            };
            for (_, sender) in pending {
                let _ = sender.send(event::ApprovalResponse::Deny);
            }
            bus.send(AppEvent::Interrupted {
                session_id: local_session_id.clone(),
                reason: "user requested".into(),
            });
            slog(&session_log, |l| l.info("Agent loop interrupted"));
            return Ok((loop_stats, LoopExitReason::Interrupted));
        }
        // Check budget before sending
        if conversation.remaining_budget() <= MIN_BUDGET_TOKENS {
            let remaining = conversation.remaining_budget();
            slog(&session_log, |l| {
                l.warn(&format!(
                    "Budget exhausted ({} tokens remaining)",
                    remaining
                ))
            });
            bus.send(AppEvent::BudgetExhausted { remaining });
            exit_reason = LoopExitReason::BudgetExhausted;
            break;
        }

        // Drain context injection queue (display takeover messages, presence
        // interjections, steer fallbacks, etc.). Steer entries (tagged with
        // `steer_id`) are surfaced as `[User]` so the model reads them as
        // user direction; everything else uses the `[System]` prefix it has
        // always used.
        if let Ok(mut q) = context_injection.lock() {
            for inj in q.drain(..) {
                let prefix = if inj.steer_id.is_some() {
                    "User"
                } else {
                    "System"
                };
                let text = format!("[{}] {}", prefix, inj.text);
                if inj.images.is_empty() {
                    conversation.add_user(text.clone());
                } else {
                    conversation.add_user_with_images(text.clone(), inj.images);
                }
                slog(&session_log, |l| {
                    l.info(&format!("Context injected: {}", inj.text))
                });
                if let Some(id) = inj.steer_id {
                    bus.send(AppEvent::SteerDelivered {
                        session_id: local_session_id.clone(),
                        id,
                        mid_turn: false,
                    });
                }
            }
        }

        conversation.increment_turn();
        let budget_pct = conversation.usage_fraction() * 100.0;
        let remaining = conversation.remaining_budget();

        slog(&session_log, |l| l.turn_start(turn, budget_pct, remaining));

        bus.send(AppEvent::TurnStarted {
            session_id: local_session_id.clone(),
            turn,
            budget_pct,
            remaining,
        });

        // When CU is enabled, the OpenAI computer tool rejects multiple images.
        // Strip all but the most recent screenshot before each API call so the
        // logged context matches the payload sent to the model.
        if provider.cu_enabled() {
            conversation.strip_old_images();
        }

        // Log the full messages array being sent to the API
        slog(&session_log, |l| {
            if let Ok(json) = serde_json::to_string_pretty(conversation.messages()) {
                l.messages_input(&json);
            }
        });
        match provider.request_snapshot(conversation.messages(), true) {
            Ok((context_format, raw_context)) => {
                bus.send(AppEvent::ContextSnapshot {
                    session_id: local_session_id.clone(),
                    source: "native".to_string(),
                    label: "Internal agent request payload".to_string(),
                    request_id: Some(format!("native-turn-{turn}")),
                    request_index: Some(turn as u64),
                    turn: Some(turn),
                    format: context_format,
                    token_count: conversation.last_usage().map(|u| u.total_tokens),
                    token_count_kind: None,
                    context_window: Some(conversation.context_window()),
                    hard_context_window: Some(conversation.context_window()),
                    item_count: provider_request_item_count(&raw_context),
                    raw: raw_context,
                });
            }
            Err(e) => {
                slog(&session_log, |l| {
                    l.warn(&format!(
                        "Failed to build provider request context snapshot: {}",
                        e
                    ))
                });
            }
        }

        // Streaming API call — wrapped in select! so an interrupt cancels
        // mid-stream without waiting for the provider to finish. The
        // interrupt branch returns `None` so the surrounding block can
        // handle drain-and-exit identically to the top-of-loop check.
        let response_opt: Option<provider::ChatResponse> = {
            const STREAM_RETRIES: u32 = 3;
            let mut last_stream_err = None;
            let mut resp = None;
            let mut was_cancelled = false;
            for attempt in 0..=STREAM_RETRIES {
                let stream_bus = bus.clone();
                let stream_session_id = local_session_id.clone();
                let on_stream_event = move |event: crate::provider::StreamEvent| {
                    if let crate::provider::StreamEvent::Delta(ref text) = event {
                        stream_bus.send(AppEvent::ModelResponseDelta {
                            session_id: stream_session_id.clone(),
                            text: text.clone(),
                        });
                    }
                };
                let stream_fut = provider.chat_stream(conversation.messages(), &on_stream_event);
                let outcome = tokio::select! {
                    biased;
                    _ = cancel_token.cancelled() => {
                        was_cancelled = true;
                        break;
                    }
                    r = stream_fut => r,
                };
                match outcome {
                    Ok(r) => {
                        resp = Some(r);
                        break;
                    }
                    Err(e) => {
                        let is_stream_error = e.to_string().contains("Stream error");
                        if is_stream_error && attempt < STREAM_RETRIES {
                            slog(&session_log, |l| {
                                l.warn(&format!(
                                    "Stream error (attempt {}/{}), retrying: {}",
                                    attempt + 1,
                                    STREAM_RETRIES + 1,
                                    e
                                ))
                            });
                            let delay = std::time::Duration::from_millis(
                                1000 * 2u64.pow(attempt) + (turn as u64 % 500),
                            );
                            // Retries are also interruptible — don't sit in
                            // a sleep while the user is trying to cancel.
                            tokio::select! {
                                biased;
                                _ = cancel_token.cancelled() => {
                                    was_cancelled = true;
                                    break;
                                }
                                _ = tokio::time::sleep(delay) => {}
                            }
                            last_stream_err = Some(e);
                            continue;
                        }
                        slog(&session_log, |l| l.error(&format!("API error: {}", e)));
                        bus.send(AppEvent::LoopError(e.to_string()));
                        return Err(e);
                    }
                }
            }
            if was_cancelled {
                None
            } else {
                match resp {
                    Some(r) => Some(r),
                    None => {
                        let e = last_stream_err.unwrap_or_else(|| {
                            CallerError::Provider("Stream failed after retries".to_string())
                        });
                        slog(&session_log, |l| l.error(&format!("API error: {}", e)));
                        bus.send(AppEvent::LoopError(e.to_string()));
                        return Err(e);
                    }
                }
            }
        };

        // Cancelled mid-stream → drain approvals and exit via Interrupted.
        let response = match response_opt {
            Some(r) => r,
            None => {
                let pending: Vec<_> = {
                    let mut reg = approval_registry.lock().unwrap();
                    reg.drain().collect()
                };
                for (_, sender) in pending {
                    let _ = sender.send(event::ApprovalResponse::Deny);
                }
                bus.send(AppEvent::Interrupted {
                    session_id: local_session_id.clone(),
                    reason: "user requested".into(),
                });
                slog(&session_log, |l| {
                    l.info("Agent loop interrupted mid-stream")
                });
                return Ok((loop_stats, LoopExitReason::Interrupted));
            }
        };
        conversation.set_usage(response.usage.clone());

        // Auto-compact when context usage exceeds 90%
        if conversation.auto_compact() {
            slog(&session_log, |l| {
                l.info(&format!("Auto-compacted conversation at turn {}", turn))
            });
            bus.send(AppEvent::ContextManagement { turn });
        }

        loop_stats.turns = turn;
        loop_stats.usage.prompt_tokens += response.usage.prompt_tokens;
        loop_stats.usage.completion_tokens += response.usage.completion_tokens;
        loop_stats.usage.total_tokens += response.usage.total_tokens;
        if !response.content.is_empty() {
            loop_stats.last_response = Some(response.content.clone());
        }

        // Store assistant message — with or without tool calls
        let has_tool_calls = !response.tool_calls.is_empty();
        let has_cu_calls = !response.cu_calls.is_empty();
        if has_tool_calls || has_cu_calls {
            let refs: Vec<conversation::ToolCallRef> = response
                .tool_calls
                .iter()
                .map(|tc| conversation::ToolCallRef {
                    id: tc.id.clone(),
                    call_id: tc.call_id.clone(),
                    name: tc.name.clone(),
                    arguments: tc.arguments.clone(),
                })
                .collect();
            conversation.add_assistant_tool_calls(
                response.content.clone(),
                refs,
                response.raw_output.clone(),
            );
        } else {
            conversation.add_assistant(response.content.clone());
        }

        // Log the full model response (no truncation)
        slog(&session_log, |l| {
            l.model_response(
                &response.content,
                response.usage.prompt_tokens,
                response.usage.completion_tokens,
                response.usage.total_tokens,
                response.usage.cached_tokens,
                None,
            )
        });

        // Log reasoning content if available
        if response.reasoning_summary.is_some() || response.reasoning_content.is_some() {
            slog(&session_log, |l| {
                l.reasoning_content(
                    response.reasoning_summary.as_deref(),
                    response.reasoning_content.as_deref(),
                )
            });
        }

        // Check budget warning
        if !budget_warning_shown && conversation.usage_fraction() >= BUDGET_WARNING_THRESHOLD {
            let pct = conversation.usage_fraction() * 100.0;
            let remaining = conversation.remaining_budget();
            slog(&session_log, |l| {
                l.warn(&format!(
                    "Budget warning: {:.0}% used, {} remaining",
                    pct, remaining
                ))
            });
            bus.send(AppEvent::BudgetWarning { pct, remaining });
            budget_warning_shown = true;
        }

        // For CU-only turns, synthesize a content summary from the actions
        let display_content = if response.content.is_empty() && has_cu_calls {
            let descs: Vec<String> = response
                .cu_calls
                .iter()
                .flat_map(|cu| {
                    cu.actions.iter().map(|a| match a {
                        computer_use::CuAction::Click { x, y, .. } => format!("click({},{})", x, y),
                        computer_use::CuAction::DoubleClick { x, y, .. } => {
                            format!("double_click({},{})", x, y)
                        }
                        computer_use::CuAction::Type { text } => {
                            format!("type(\"{}\")", types::truncate_str(text, 30))
                        }
                        computer_use::CuAction::Key { key } => format!("key({})", key),
                        computer_use::CuAction::Scroll { x, y, .. } => {
                            format!("scroll({},{})", x, y)
                        }
                        computer_use::CuAction::Screenshot => "screenshot".to_string(),
                        computer_use::CuAction::Wait { .. } => "wait".to_string(),
                        _ => format!("{:?}", a),
                    })
                })
                .collect();
            descs.join(" → ")
        } else {
            response.content.clone()
        };

        bus.send(AppEvent::ModelResponse {
            session_id: local_session_id.clone(),
            turn,
            content: display_content,
            usage: response.usage.clone(),
            reasoning: response.reasoning_summary.clone(),
            source: None,
        });

        // ====== TOOL CALL PATH vs TEXT EXTRACTION PATH ======
        if has_tool_calls {
            // --- Native tool call path ---
            let batch = assemble_batch_from_tool_calls(&response.tool_calls);

            // Call IDs answered by a dedicated handler below. Every later
            // catch-all result loop must skip these — a second result for the
            // same tool_use_id is rejected by strict providers (Anthropic).
            let mut handled_call_ids: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            for (call_id, tool_name, result_text) in &batch.precomputed_results {
                conversation.add_tool_result(call_id, tool_name, result_text);
                handled_call_ids.insert(call_id.clone());
            }

            // Apply context directives from manage_context tool call
            if let Some(ref ctx) = batch.context_directives {
                if let Some(drops) = ctx.get("drop_turns").and_then(|d| d.as_array()) {
                    let indices: Vec<usize> = drops
                        .iter()
                        .filter_map(|v| v.as_u64().map(|n| n as usize))
                        .collect();
                    conversation.drop_turns(&indices);
                }
                if let Some(summarize) = ctx.get("summarize") {
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
                slog(&session_log, |l| {
                    l.debug("Context directives applied (tool call)")
                });
            }

            // Record a structured sub-agent result (submit_result) before
            // the done check: "submit_result + signal_done" in one batch is
            // the natural final move for a child and must not lose the
            // result to the done short-circuit.
            for (call_id, args) in &batch.sub_agent_results {
                handled_call_ids.insert(call_id.clone());
                let response = handle_submit_result_call(args, orchestration, &local_session_id);
                conversation.add_tool_result(call_id, "submit_result", &response);
            }

            // Check done signal
            if batch.is_done {
                slog(&session_log, |l| {
                    l.info(&format!(
                        "Done signal received (tool call): {}",
                        batch.done_message.as_deref().unwrap_or("(no message)")
                    ))
                });
                // Send tool results for all calls including signal_done
                for (call_id, tool_name, _) in map_results_to_tool_responses(
                    "",
                    "",
                    &batch.nonce_to_call_id,
                    &batch.call_id_names,
                ) {
                    if handled_call_ids.contains(&call_id) {
                        continue;
                    }
                    conversation.add_tool_result(&call_id, &tool_name, "OK");
                }
                bus.send(AppEvent::DoneSignal {
                    session_id: local_session_id.clone(),
                    message: batch.done_message.clone(),
                });
                exit_reason = LoopExitReason::DoneSignal;
                break;
            }

            // Process MCP tool calls (if any)
            if !batch.mcp_calls.is_empty() {
                if let Some(mgr) = mcp_mgr {
                    for (call_id, tool_name, args_json) in &batch.mcp_calls {
                        let args: serde_json::Value =
                            serde_json::from_str(args_json).unwrap_or_default();
                        let result = mgr.call_tool(tool_name, args).await;
                        let output = match result {
                            Ok(text) => text,
                            Err(e) => format!("MCP tool error: {}", e),
                        };
                        conversation.add_tool_result(call_id, tool_name, &output);
                        handled_call_ids.insert(call_id.clone());
                    }
                } else {
                    for (call_id, tool_name, _) in &batch.mcp_calls {
                        conversation.add_tool_result(
                            call_id,
                            tool_name,
                            "Error: MCP client not configured",
                        );
                        handled_call_ids.insert(call_id.clone());
                    }
                }
            }

            // Process invoke_skill tool calls (if any)
            for (call_id, skill_name, arguments) in &batch.skill_invocations {
                handled_call_ids.insert(call_id.clone());
                let discovered = skills::discover_skills(Some(&project.root));
                match discovered.iter().find(|s| s.config.name == *skill_name) {
                    Some(skill) => {
                        let body = skills::load_skill_body(skill, arguments);
                        slog(&session_log, |l| {
                            l.info(&format!(
                                "Invoked skill '{}' (args: {})",
                                skill_name,
                                if arguments.is_empty() {
                                    "(none)"
                                } else {
                                    arguments
                                }
                            ))
                        });
                        conversation.add_tool_result(
                            call_id,
                            "invoke_skill",
                            &format!(
                                "Skill '{}' loaded. Follow these instructions:\n\n{}",
                                skill_name, body
                            ),
                        );
                    }
                    None => {
                        let available: Vec<&str> =
                            discovered.iter().map(|s| s.config.name.as_str()).collect();
                        conversation.add_tool_result(
                            call_id,
                            "invoke_skill",
                            &format!(
                                "Error: skill '{}' not found. Available: {}",
                                skill_name,
                                if available.is_empty() {
                                    "(none)".to_string()
                                } else {
                                    available.join(", ")
                                }
                            ),
                        );
                    }
                }
            }

            // Spawn supervised sub-agent sessions (spawn_sub_agent).
            for (call_id, args) in &batch.sub_agent_spawns {
                handled_call_ids.insert(call_id.clone());
                let response =
                    handle_spawn_sub_agent_call(args, orchestration, project, &session_log).await;
                conversation.add_tool_result(call_id, "spawn_sub_agent", &response);
            }

            // Await sub-agent completions (wait_sub_agents). Blocking:
            // resolves inside this tool call, honoring interrupt/stop.
            for (call_id, args) in &batch.sub_agent_waits {
                handled_call_ids.insert(call_id.clone());
                let response = handle_wait_sub_agents_call(
                    args,
                    orchestration,
                    bus,
                    &local_session_id,
                    &session_log,
                )
                .await;
                conversation.add_tool_result(call_id, "wait_sub_agents", &response);
            }

            // Handle shared_view tool calls (dashboard coordination layer)
            if !batch.shared_view_calls.is_empty() {
                for (call_id, _) in &batch.shared_view_calls {
                    handled_call_ids.insert(call_id.clone());
                }
                handle_shared_view_calls(
                    &batch.shared_view_calls,
                    conversation,
                    bus,
                    &autonomy,
                    session_registry,
                    local_session_id.clone(),
                    log_dir,
                    &mut cu_action_counter,
                    &session_log,
                )
                .await;
            }

            // Handle live audio spawn requests (blocking)
            for (call_id, session_id, args) in &batch.live_audio_spawns {
                handled_call_ids.insert(call_id.clone());
                let spec_result =
                    serde_json::from_value::<live_audio_types::LiveAudioSpec>(args.clone());
                match spec_result {
                    Ok(mut spec) => {
                        let system_prompt = prompts::build_live_audio_prompt(
                            &spec.playbook,
                            &spec.response_schema,
                            Some(&project.root),
                        );
                        spec.playbook = system_prompt;

                        let api_key_var = match spec.provider {
                            live_audio_types::LiveAudioProvider::Gemini => "GEMINI_API_KEY",
                            live_audio_types::LiveAudioProvider::OpenAI => "OPENAI_API_KEY",
                        };
                        let api_key = match std::env::var(api_key_var) {
                            Ok(k) => k,
                            Err(_) => {
                                conversation.add_tool_result(
                                    call_id,
                                    "spawn_live_audio",
                                    &format!("Error: {} not set", api_key_var),
                                );
                                continue;
                            }
                        };

                        let mut bridge = if platform::vortex_audio_shm_available() {
                            audio_routing::create_vortex_bridge()
                        } else {
                            match audio_routing::create_bridge(session_id).await {
                                Ok(b) => b,
                                Err(e) => {
                                    conversation.add_tool_result(
                                        call_id,
                                        "spawn_live_audio",
                                        &format!("Error creating audio bridge: {}", e),
                                    );
                                    continue;
                                }
                            }
                        };

                        if !bridge.uses_vortex_shm() {
                            if let Err(e) = audio_routing::set_as_default(&mut bridge).await {
                                slog(&session_log, |l| {
                                    l.warn(&format!("Could not set audio bridge as default: {}", e))
                                });
                            }
                        }

                        slog(&session_log, |l| {
                            l.info(&format!(
                                "Live audio session '{}' starting ({:?})",
                                session_id, spec.provider
                            ))
                        });

                        let result = live_audio::run_session(
                            &spec,
                            &api_key,
                            &bridge,
                            log_dir,
                            Some(bus),
                            &project.config.transcription,
                        )
                        .await;

                        drop(bridge);

                        match result {
                            Ok(la_result) => {
                                let result_json = serde_json::to_string_pretty(&la_result)
                                    .unwrap_or_else(|_| format!("{:?}", la_result));
                                conversation.add_tool_result(
                                    call_id,
                                    "spawn_live_audio",
                                    &result_json,
                                );
                            }
                            Err(e) => {
                                conversation.add_tool_result(
                                    call_id,
                                    "spawn_live_audio",
                                    &format!("Error: {}", e),
                                );
                            }
                        }
                    }
                    Err(e) => {
                        conversation.add_tool_result(
                            call_id,
                            "spawn_live_audio",
                            &format!("Error parsing LiveAudioSpec: {}", e),
                        );
                    }
                }
            }

            if batch.agent_input_json.is_none() && !batch.precomputed_results.is_empty() {
                continue;
            }

            // If no runtime commands, just respond to tool calls with context update
            let Some(ref json_str) = batch.agent_input_json else {
                empty_command_streak = 0;
                // Respond to whatever no dedicated handler answered above
                // (manage_context, or an empty batch).
                for (call_id, tool_name) in &batch.call_id_names {
                    if handled_call_ids.contains(call_id)
                        || mcp_client::McpClientManager::is_mcp_tool(tool_name)
                    {
                        continue;
                    }
                    conversation.add_tool_result(call_id, tool_name, "OK — context updated.");
                }
                continue;
            };
            empty_command_streak = 0;

            // Inject project context and normalize
            let json_str = normalize_command_batch(&inject_project_context(json_str, project));

            // Headless askHuman check — skip unless JSON mode (which handles it via stdin)
            if headless && json_approval.is_none() && has_ask_human_command(&json_str) {
                slog(&session_log, |l| {
                    l.warn("askHuman requested in headless mode; prompting model to continue")
                });
                for (call_id, tool_name) in &batch.call_id_names {
                    if handled_call_ids.contains(call_id) {
                        continue;
                    }
                    conversation.add_tool_result(
                        call_id,
                        tool_name,
                        "askHuman is unavailable in headless mode. Proceed with assumptions.",
                    );
                }
                continue;
            }
            // In JSON mode, emit the question so the outbound broadcaster prints it
            if json_approval.is_some() {
                if let Some(question) = extract_ask_human_question(&json_str) {
                    bus.send(AppEvent::HumanQuestionDetected { question });
                }
            }

            // Autonomy / approval check (same as text path)
            let needs_approval = {
                let classifications = autonomy::classify_batch(&json_str);
                let autonomy_state = autonomy.read().await;
                let mut need: Option<(autonomy::ActionCategory, bool)> = None;
                for (_idx, categories) in &classifications {
                    for &cat in categories {
                        if cat == autonomy::ActionCategory::HumanInput {
                            continue;
                        }
                        let rule = autonomy_state.rules.rule_for(cat);
                        if matches!(rule, autonomy::ApprovalRule::Deny) {
                            if need.is_none_or(|(prev, _)| cat.severity() > prev.severity()) {
                                need = Some((cat, true));
                            }
                        } else if autonomy_state.needs_approval(cat)
                            && need.is_none_or(|(prev, was_deny)| {
                                !was_deny && cat.severity() > prev.severity()
                            })
                        {
                            need = Some((cat, false));
                        }
                    }
                }
                need
            };

            let mut should_skip = false;
            if let Some((cat, denied_by_policy)) = needs_approval {
                let preview = format_command_preview(&json_str);

                // Dedup: skip approval for retries of already-approved commands
                if !denied_by_policy && autonomy.read().await.was_command_approved(&preview) {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "dedup-auto-approved")
                    });
                } else {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "waiting")
                    });

                    if denied_by_policy {
                        slog(&session_log, |l| {
                            l.approval(&cat.to_string(), &preview, "denied-policy")
                        });
                        bus.send(AppEvent::TaskComplete {
                            session_id: local_session_id.clone(),
                            reason: format!("Denied by policy ({})", cat),
                            summary: None,
                        });
                        return Ok((loop_stats, LoopExitReason::Denied));
                    }

                    if let Some(slot) = json_approval {
                        // JSON mode: emit approval event and wait for stdin response
                        bus.send(AppEvent::ApprovalRequired {
                            session_id: local_session_id.clone(),
                            id: turn as u64,
                            command_preview: preview.clone(),
                            category: cat,
                        });
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            let mut guard = slot.lock().unwrap();
                            *guard = Some((turn as u64, tx));
                        }
                        match rx.await {
                            Ok(event::ApprovalResponse::Approve) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approved")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "approve".to_string(),
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::Approve,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::ApproveAll) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approve-all")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "approve_all".to_string(),
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::ApproveAll,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::Skip) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "skipped")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "skip".to_string(),
                                });
                                should_skip = true;
                            }
                            Ok(event::ApprovalResponse::Deny) | Err(_) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "denied")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "deny".to_string(),
                                });
                                bus.send(AppEvent::TaskComplete {
                                    session_id: local_session_id.clone(),
                                    reason: "Denied by user".to_string(),
                                    summary: None,
                                });
                                return Ok((loop_stats, LoopExitReason::Denied));
                            }
                        }
                    } else if headless {
                        slog(&session_log, |l| {
                            l.approval(&cat.to_string(), &preview, "denied-no-approver")
                        });
                        bus.send(AppEvent::TaskComplete {
                            session_id: local_session_id.clone(),
                            reason: format!("Approval required in headless mode ({})", cat),
                            summary: None,
                        });
                        return Ok((loop_stats, LoopExitReason::Denied));
                    } else {
                        // Interactive mode (TUI/MCP): approval via registry
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        approval_registry.lock().unwrap().insert(turn as u64, tx);
                        bus.send(AppEvent::ApprovalRequired {
                            session_id: local_session_id.clone(),
                            id: turn as u64,
                            command_preview: preview.clone(),
                            category: cat,
                        });
                        match rx.await {
                            Ok(event::ApprovalResponse::Approve) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approved")
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::Approve,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::ApproveAll) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approve-all")
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::ApproveAll,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::Skip) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "skipped")
                                });
                                should_skip = true;
                            }
                            Ok(event::ApprovalResponse::Deny) | Err(_) => {
                                // Distinguish a real user deny from an interrupt
                                // that caused the watcher to drain the registry
                                // with Deny as a synthetic response. Interrupt
                                // takes precedence so the phase/exit reason
                                // reflects what actually happened.
                                if cancel_token.is_cancelled() {
                                    bus.send(AppEvent::Interrupted {
                                        session_id: local_session_id.clone(),
                                        reason: "user requested".into(),
                                    });
                                    slog(&session_log, |l| {
                                        l.info("Agent loop interrupted during approval wait")
                                    });
                                    return Ok((loop_stats, LoopExitReason::Interrupted));
                                }
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "denied")
                                });
                                bus.send(AppEvent::TaskComplete {
                                    session_id: local_session_id.clone(),
                                    reason: "Denied by user".to_string(),
                                    summary: None,
                                });
                                return Ok((loop_stats, LoopExitReason::Denied));
                            }
                        }
                    }
                } // close dedup else block
            } else {
                // Commands auto-approved — log for visibility at Normal verbosity
                let preview = format_command_preview(&json_str);
                if !preview.is_empty() {
                    bus.send(AppEvent::AutoApproved {
                        preview: preview.clone(),
                    });
                }
            }

            if should_skip {
                for (call_id, tool_name) in &batch.call_id_names {
                    if handled_call_ids.contains(call_id) {
                        continue;
                    }
                    conversation.add_tool_result(call_id, tool_name, "Command skipped by user.");
                }
                continue;
            }

            // Run agent
            slog(&session_log, |l| l.agent_input(&json_str));
            maybe_auto_launch_xvfb(&json_str, xvfb_guard, provider.name(), &session_log).await;
            let preview = format_commands_preview(&json_str);
            bus.send(AppEvent::AgentStarted {
                session_id: local_session_id.clone(),
                turn,
                commands_preview: preview.clone(),
                item_id: None,
                source: None,
            });

            let output = agent_runner::run_agent(&json_str, log_dir).await?;
            let output_id = event::next_agent_output_id();

            // Log agent output
            slog(&session_log, |l| {
                l.agent_output_with_id(&output.stdout, &output.stderr, None, Some(&output_id))
            });

            bus.send(AppEvent::AgentOutput {
                session_id: local_session_id.clone(),
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
                source: None,
                output_id: Some(output_id),
            });

            // Map results back to individual tool responses
            let tool_results = map_results_to_tool_responses(
                &output.stdout,
                &output.stderr,
                &batch.nonce_to_call_id,
                &batch.call_id_names,
            );
            let budget = conversation.budget_summary();
            for (call_id, tool_name, result_text) in &tool_results {
                if handled_call_ids.contains(call_id) {
                    continue;
                }
                let text = format!("{}\n\n{}", result_text, budget);
                if tool_name == "capture_screen" {
                    if let Some(images) = encode_screenshot(result_text) {
                        conversation.add_tool_result_with_images(call_id, tool_name, &text, images);
                        continue;
                    }
                }
                conversation.add_tool_result(call_id, tool_name, &text);
            }

            // Process CU calls alongside function tool calls
            if has_cu_calls {
                execute_cu_calls(
                    &response.cu_calls,
                    conversation,
                    provider.cu_display(),
                    log_dir,
                    &mut cu_action_counter,
                    &session_log,
                    session_registry,
                )
                .await;
            }
        } else if has_cu_calls {
            // CU-only turn (no function tool calls)
            execute_cu_calls(
                &response.cu_calls,
                conversation,
                provider.cu_display(),
                log_dir,
                &mut cu_action_counter,
                &session_log,
                session_registry,
            )
            .await;
        } else {
            // --- Legacy text extraction path ---

            // Extract JSON from response
            let json_str = match extract_json(&response.content) {
                Some(json) => json.to_string(),
                None => {
                    slog(&session_log, |l| {
                        l.info("No JSON found in response — task complete")
                    });
                    let brief: String = response.content.chars().take(500).collect();
                    bus.send(AppEvent::TaskComplete {
                        session_id: local_session_id.clone(),
                        reason: "Task complete".to_string(),
                        summary: if brief.is_empty() {
                            None
                        } else {
                            Some(brief.clone())
                        },
                    });
                    exit_reason = LoopExitReason::TaskComplete;
                    break;
                }
            };

            slog(&session_log, |l| l.json_extracted(&json_str));

            bus.send(AppEvent::JsonExtracted {
                preview: json_str.chars().take(100).collect(),
            });

            // Check for explicit done signal (used in structured output / JSON mode)
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json_str) {
                if parsed
                    .get("done")
                    .and_then(|d| d.as_bool())
                    .unwrap_or(false)
                {
                    let message = parsed
                        .get("message")
                        .and_then(|m| m.as_str())
                        .map(String::from);
                    slog(&session_log, |l| {
                        l.info(&format!(
                            "Done signal received: {}",
                            message.as_deref().unwrap_or("(no message)")
                        ))
                    });
                    bus.send(AppEvent::DoneSignal {
                        session_id: local_session_id.clone(),
                        message: message.clone(),
                    });
                    exit_reason = LoopExitReason::DoneSignal;
                    break;
                }
            }

            // Apply context directives (drop_turns, summarize) before sending to agent
            let (json_str, had_context) = apply_context_directives(&json_str, conversation);

            if had_context {
                slog(&session_log, |l| l.debug("Context directives applied"));
            }

            // No commands to execute
            if json_str.is_empty() {
                if had_context {
                    empty_command_streak = 0;
                    slog(&session_log, |l| {
                        l.debug(&format!("Turn {}: context management only", turn))
                    });
                    bus.send(AppEvent::ContextManagement { turn });
                    conversation.add_user("Context updated.".to_string());
                    continue;
                } else {
                    empty_command_streak += 1;
                    if empty_command_streak >= 2 {
                        slog(&session_log, |l| {
                            l.info("No commands across consecutive turns — task complete")
                        });
                        let brief: String = response.content.chars().take(500).collect();
                        bus.send(AppEvent::TaskComplete {
                            session_id: local_session_id.clone(),
                            reason: "Task complete".to_string(),
                            summary: if brief.is_empty() {
                                None
                            } else {
                                Some(brief.clone())
                            },
                        });
                        exit_reason = LoopExitReason::TaskComplete;
                        break;
                    }
                    slog(&session_log, |l| {
                        l.warn(
                            "No commands and no context directives — requesting explicit done signal",
                        )
                    });
                    conversation.add_user(
                        "No commands were produced. If the task is complete, respond with JSON containing done=true. Otherwise provide commands.".to_string(),
                    );
                    continue;
                }
            }
            empty_command_streak = 0;

            // Inject project context (memory_file) into commands and normalize aliases.
            let json_str = normalize_command_batch(&inject_project_context(&json_str, project));

            // In headless mode there is no askHuman input panel — skip unless JSON mode.
            if headless && json_approval.is_none() && has_ask_human_command(&json_str) {
                slog(&session_log, |l| {
                    l.warn("askHuman requested in headless mode; prompting model to continue")
                });
                conversation.add_user(
                    "askHuman is unavailable in headless mode (--no-tui or non-interactive stdin). \
Proceed with explicit assumptions and continue without additional questions."
                        .to_string(),
                );
                continue;
            }
            // In JSON mode, emit the question so the outbound broadcaster prints it
            if json_approval.is_some() {
                if let Some(question) = extract_ask_human_question(&json_str) {
                    bus.send(AppEvent::HumanQuestionDetected { question });
                }
            }

            // Check autonomy / approval for commands
            let needs_approval = {
                let classifications = autonomy::classify_batch(&json_str);
                let autonomy_state = autonomy.read().await;
                let mut need: Option<(autonomy::ActionCategory, bool)> = None;
                for (_idx, categories) in &classifications {
                    for &cat in categories {
                        if cat == autonomy::ActionCategory::HumanInput {
                            continue;
                        }
                        let rule = autonomy_state.rules.rule_for(cat);
                        if matches!(rule, autonomy::ApprovalRule::Deny) {
                            if need.is_none_or(|(prev, _)| cat.severity() > prev.severity()) {
                                need = Some((cat, true));
                            }
                        } else if autonomy_state.needs_approval(cat)
                            && need.is_none_or(|(prev, was_deny)| {
                                !was_deny && cat.severity() > prev.severity()
                            })
                        {
                            need = Some((cat, false));
                        }
                    }
                }
                need
            };

            let mut should_skip = false;
            if let Some((cat, denied_by_policy)) = needs_approval {
                let preview = format_command_preview(&json_str);

                // Dedup: skip approval for retries of already-approved commands
                if !denied_by_policy && autonomy.read().await.was_command_approved(&preview) {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "dedup-auto-approved")
                    });
                } else {
                    slog(&session_log, |l| {
                        l.approval(&cat.to_string(), &preview, "waiting")
                    });

                    if denied_by_policy {
                        slog(&session_log, |l| {
                            l.approval(&cat.to_string(), &preview, "denied-policy")
                        });
                        bus.send(AppEvent::TaskComplete {
                            session_id: local_session_id.clone(),
                            reason: format!("Denied by policy ({})", cat),
                            summary: None,
                        });
                        return Ok((loop_stats, LoopExitReason::Denied));
                    }

                    if let Some(slot) = json_approval {
                        // JSON mode: emit approval event and wait for stdin response
                        bus.send(AppEvent::ApprovalRequired {
                            session_id: local_session_id.clone(),
                            id: turn as u64,
                            command_preview: preview.clone(),
                            category: cat,
                        });
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        {
                            let mut guard = slot.lock().unwrap();
                            *guard = Some((turn as u64, tx));
                        }
                        match rx.await {
                            Ok(event::ApprovalResponse::Approve) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approved")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "approve".to_string(),
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::Approve,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::ApproveAll) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approve-all")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "approve_all".to_string(),
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::ApproveAll,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::Skip) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "skipped")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "skip".to_string(),
                                });
                                should_skip = true;
                            }
                            Ok(event::ApprovalResponse::Deny) | Err(_) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "denied")
                                });
                                bus.send(AppEvent::ApprovalResolved {
                                    session_id: local_session_id.clone(),
                                    id: turn as u64,
                                    action: "deny".to_string(),
                                });
                                bus.send(AppEvent::TaskComplete {
                                    session_id: local_session_id.clone(),
                                    reason: "Denied by user".to_string(),
                                    summary: None,
                                });
                                return Ok((loop_stats, LoopExitReason::Denied));
                            }
                        }
                    } else if headless {
                        slog(&session_log, |l| {
                            l.approval(&cat.to_string(), &preview, "denied-no-approver")
                        });
                        bus.send(AppEvent::TaskComplete {
                            session_id: local_session_id.clone(),
                            reason: format!("Approval required in headless mode ({})", cat),
                            summary: None,
                        });
                        return Ok((loop_stats, LoopExitReason::Denied));
                    } else {
                        // Interactive mode (TUI/MCP): approval via registry
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        approval_registry.lock().unwrap().insert(turn as u64, tx);
                        bus.send(AppEvent::ApprovalRequired {
                            session_id: local_session_id.clone(),
                            id: turn as u64,
                            command_preview: preview.clone(),
                            category: cat,
                        });
                        match rx.await {
                            Ok(event::ApprovalResponse::Approve) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approved")
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::Approve,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::ApproveAll) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "approve-all")
                                });
                                apply_user_approval(
                                    event::ApprovalResponse::ApproveAll,
                                    cat,
                                    &preview,
                                    &autonomy,
                                    bus,
                                )
                                .await;
                            }
                            Ok(event::ApprovalResponse::Skip) => {
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "skipped")
                                });
                                should_skip = true;
                            }
                            Ok(event::ApprovalResponse::Deny) | Err(_) => {
                                // Distinguish a real user deny from an interrupt
                                // that caused the watcher to drain the registry
                                // with Deny as a synthetic response. Interrupt
                                // takes precedence so the phase/exit reason
                                // reflects what actually happened.
                                if cancel_token.is_cancelled() {
                                    bus.send(AppEvent::Interrupted {
                                        session_id: local_session_id.clone(),
                                        reason: "user requested".into(),
                                    });
                                    slog(&session_log, |l| {
                                        l.info("Agent loop interrupted during approval wait")
                                    });
                                    return Ok((loop_stats, LoopExitReason::Interrupted));
                                }
                                slog(&session_log, |l| {
                                    l.approval(&cat.to_string(), &preview, "denied")
                                });
                                bus.send(AppEvent::TaskComplete {
                                    session_id: local_session_id.clone(),
                                    reason: "Denied by user".to_string(),
                                    summary: None,
                                });
                                return Ok((loop_stats, LoopExitReason::Denied));
                            }
                        }
                    }
                } // close dedup else block
            } else {
                // Commands auto-approved — log for visibility at Normal verbosity
                let preview = format_command_preview(&json_str);
                if !preview.is_empty() {
                    bus.send(AppEvent::AutoApproved {
                        preview: preview.clone(),
                    });
                }
            }

            if should_skip {
                conversation.add_user("Command skipped by user.".to_string());
                continue;
            }

            // Log the full JSON being sent to the agent
            slog(&session_log, |l| l.agent_input(&json_str));
            maybe_auto_launch_xvfb(&json_str, xvfb_guard, provider.name(), &session_log).await;

            let preview = format_commands_preview(&json_str);
            bus.send(AppEvent::AgentStarted {
                session_id: local_session_id.clone(),
                turn,
                commands_preview: preview.clone(),
                item_id: None,
                source: None,
            });

            let output = agent_runner::run_agent(&json_str, log_dir).await?;
            let output_id = event::next_agent_output_id();

            // Log agent output
            slog(&session_log, |l| {
                l.agent_output_with_id(&output.stdout, &output.stderr, None, Some(&output_id))
            });

            bus.send(AppEvent::AgentOutput {
                session_id: local_session_id.clone(),
                stdout: output.stdout.clone(),
                stderr: output.stderr.clone(),
                source: None,
                output_id: Some(output_id),
            });

            // Format agent output as next user message, include budget summary
            let mut user_msg = format!("Agent output:\n{}", output.stdout);
            if !output.stderr.is_empty() {
                user_msg.push_str(&format!("\nStderr:\n{}", output.stderr));
            }
            user_msg.push_str(&format!("\n\n{}", conversation.budget_summary()));
            conversation.add_user(user_msg);
        } // end tool_calls vs text branch

        // Auto-save conversation for resume capability
        let conv_path = log_dir.join("conversation.jsonl");
        if let Err(e) = conversation.save_to_file(&conv_path) {
            slog(&session_log, |l| {
                l.debug(&format!("Failed to save conversation: {}", e))
            });
        }

        if turn == SAFETY_CAP {
            slog(&session_log, |l| {
                l.warn(&format!("Safety cap ({}) reached", SAFETY_CAP))
            });
            bus.send(AppEvent::SafetyCapReached);
            exit_reason = LoopExitReason::SafetyCapReached;
        }
    }

    slog(&session_log, |l| l.info("Agent loop finished"));
    Ok((loop_stats, exit_reason))
}

/// Wraps `run_agent_loop` in a multi-round loop that waits for follow-up messages
/// between rounds. The session continues until the user closes the channel,
/// budget is exhausted, safety cap is reached, or a non-recoverable exit occurs.
#[allow(clippy::too_many_arguments)]
async fn run_round_loop(
    provider: &dyn provider::ChatProvider,
    conversation: &mut Conversation,
    project: &Project,
    sub_agent_mode: Option<&(String, sub_agent::SubAgentRole)>,
    bus: &EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: &std::path::Path,
    mcp_mgr: Option<&mcp_client::McpClientManager>,
    follow_up_rx: &mut FollowUpReceiver,
    json_approval: Option<&JsonApprovalSlot>,
    approval_registry: &event::ApprovalRegistry,
    context_injection: &event::ContextInjectionQueue,
    session_registry: Option<&display::SharedSessionRegistry>,
    headless: bool,
    orchestration: Option<&session_supervisor::SessionOrchestration>,
) -> Result<LoopStats, CallerError> {
    let mut round = 1usize;
    let mut cumulative_stats = LoopStats::default();
    let mut xvfb_guard: Option<vision::XvfbGuard> = None;
    let local_session_id = session_log_id(&session_log);
    let mut follow_up_cancel_rx = bus.subscribe();
    let mut cancelled_follow_ups: HashSet<String> = HashSet::new();

    loop {
        let (stats, exit_reason) = run_agent_loop(
            provider,
            conversation,
            project,
            sub_agent_mode,
            bus,
            autonomy.clone(),
            session_log.clone(),
            log_dir,
            mcp_mgr,
            json_approval,
            approval_registry,
            context_injection,
            &mut xvfb_guard,
            session_registry,
            headless,
            orchestration,
        )
        .await?;

        cumulative_stats.turns += stats.turns;
        cumulative_stats.usage.prompt_tokens += stats.usage.prompt_tokens;
        cumulative_stats.usage.completion_tokens += stats.usage.completion_tokens;
        cumulative_stats.usage.total_tokens += stats.usage.total_tokens;
        cumulative_stats.rounds = round;

        // Sub-agent mode: never wait for follow-up
        if sub_agent_mode.is_some() {
            break;
        }

        // Only wait for follow-up on recoverable exits
        match exit_reason {
            LoopExitReason::DoneSignal | LoopExitReason::TaskComplete => {
                // Emit RoundComplete event. Snapshot the native conversation
                // message count so a conversation-rollback request can
                // truncate the tail back to this point.
                let turns_in_round = stats.turns;
                let native_message_count = Some(conversation.messages().len() as u32);
                bus.send(AppEvent::RoundComplete {
                    session_id: local_session_id.clone(),
                    round,
                    turns_in_round,
                    native_message_count,
                });

                // Wait for follow-up message, while accepting queued
                // cancellation requests before the next turn consumes them.
                let Some(message) = (loop {
                    while let Ok(AppEvent::FollowUpCancelRequested {
                        session_id,
                        id,
                        reason,
                    }) = follow_up_cancel_rx.try_recv()
                    {
                        if event_targets_session(&session_id, &local_session_id) {
                            record_cancelled_follow_up_id(
                                &mut cancelled_follow_ups,
                                bus,
                                local_session_id.as_deref(),
                                id,
                                &reason,
                            );
                        }
                    }
                    tokio::select! {
                        biased;
                        bus_event = follow_up_cancel_rx.recv() => {
                            match bus_event {
                                Ok(AppEvent::FollowUpCancelRequested { session_id, id, reason })
                                    if event_targets_session(&session_id, &local_session_id) =>
                                {
                                    record_cancelled_follow_up_id(
                                        &mut cancelled_follow_ups,
                                        bus,
                                        local_session_id.as_deref(),
                                        id,
                                        &reason,
                                    );
                                }
                                Ok(_) => {}
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                            }
                        }
                        maybe_message = follow_up_rx.recv() => {
                            match maybe_message {
                                Some(message) => {
                                    if follow_up_message_was_cancelled(
                                        &mut cancelled_follow_ups,
                                        &message,
                                    ) {
                                        slog(&session_log, |l| {
                                            l.info("Skipped cancelled queued follow-up")
                                        });
                                        continue;
                                    }
                                    break Some(message);
                                }
                                None => {
                                    // Channel closed — user quit or sender dropped
                                    break None;
                                }
                            }
                        }
                    }
                }) else {
                    break;
                };
                round += 1;
                let followup_text = message.attachments.text_with_file_prelude(&message.text);
                let followup_images = message.attachments.conversation_images();
                slog(&session_log, |l| {
                    l.info(&format!(
                        "Round {} follow-up: {}{}",
                        round,
                        &message.text,
                        if message.attachments.is_empty() {
                            String::new()
                        } else {
                            format!(" ({} attachment(s))", message.attachments.len())
                        }
                    ))
                });
                if followup_images.is_empty() {
                    conversation.add_user(followup_text);
                } else {
                    conversation.add_user_with_images(followup_text, followup_images);
                }
                if let Some(id) = message.steer_id {
                    bus.send(AppEvent::SteerDelivered {
                        session_id: local_session_id.clone(),
                        id,
                        mid_turn: false,
                    });
                }
                emit_follow_up_status(
                    bus,
                    local_session_id.as_deref(),
                    &message.follow_up_id,
                    Some(&message.text),
                    "delivered",
                    None,
                );
            }
            LoopExitReason::BudgetExhausted
            | LoopExitReason::SafetyCapReached
            | LoopExitReason::Denied
            | LoopExitReason::Error
            | LoopExitReason::Interrupted => {
                break;
            }
        }
    }

    Ok(cumulative_stats)
}

fn get_task_from_flags_or_env(flags: &CliFlags) -> Result<String, CallerError> {
    if let Some(ref task) = flags.task {
        return Ok(task.clone());
    }
    if let Some(ref path) = flags.task_file {
        return std::fs::read_to_string(path)
            .map(|s| s.trim_end_matches(['\r', '\n']).to_string())
            .map_err(|e| CallerError::Config(format!("Failed to read --task-file {path}: {e}")));
    }
    if let Ok(task) = env::var("INTENDANT_TASK") {
        return Ok(task);
    }
    print!("Enter task: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn resolve_initial_task_for_startup(
    flags: &CliFlags,
    web_daemon_requested: bool,
    use_tui: bool,
) -> Result<Option<String>, CallerError> {
    if web_daemon_requested {
        return Ok(None);
    }
    if flags.task_file.is_some() {
        let task = get_task_from_flags_or_env(flags)?;
        if task.is_empty() {
            return Err(CallerError::Config("No task provided".to_string()));
        }
        return Ok(Some(task));
    }
    if flags.mcp {
        return Ok(flags.task.clone().filter(|t| !t.is_empty()));
    }
    if use_tui {
        return Ok(flags.task.clone().filter(|t| !t.is_empty()));
    }
    let task = get_task_from_flags_or_env(flags)?;
    if task.is_empty() {
        return Err(CallerError::Config("No task provided".to_string()));
    }
    Ok(Some(task))
}

/// RAII guard that increments the presence-pause ref-count on construction
/// and decrements it on drop. Lets a direct-mode task pause server-side
/// narration for its own duration without clobbering pause contributions
/// from other sources (e.g. browser voice's PresenceConnected ref-count).
struct PresencePauseGuard {
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl PresencePauseGuard {
    fn new(counter: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self { counter }
    }
}

impl Drop for PresencePauseGuard {
    fn drop(&mut self) {
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Run with the presence layer mediating between user and agent loop.
///
/// The presence layer runs in its own background task, handling user input
/// and narrating agent events via `PresenceLayer::run()`. This function
/// dispatches task envelopes produced by presence to the actual agent loop.
#[allow(clippy::too_many_arguments)]
async fn run_with_presence(
    task: Option<String>,
    project: Project,
    bus: EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: PathBuf,
    user_rx: tokio::sync::mpsc::Receiver<String>,
    response_tx: tokio::sync::mpsc::Sender<String>,
    presence_event_rx: tokio::sync::mpsc::Receiver<presence::PresenceEvent>,
    agent_state: Arc<Mutex<presence::AgentStateSnapshot>>,
    _force_direct: bool,
    presence_paused: Arc<std::sync::atomic::AtomicUsize>,
    task_tx: tokio::sync::mpsc::Sender<presence::TaskEnvelope>,
    mut task_rx: tokio::sync::mpsc::Receiver<presence::TaskEnvelope>,
    approval_registry: event::ApprovalRegistry,
    frame_registry: Arc<tokio::sync::RwLock<frames::FrameRegistry>>,
    context_injection: event::ContextInjectionQueue,
    session_registry: display::SharedSessionRegistry,
    agent_backend_override: Option<external_agent::AgentBackend>,
    shared_external_agent: Arc<tokio::sync::RwLock<Option<external_agent::AgentBackend>>>,
    shared_codex_config: control_plane::SharedCodexConfig,
    shared_claude_config: control_plane::SharedClaudeConfig,
    web_port: Option<u16>,
    resume_session: Option<String>,
    resume_session_config: Option<session_config::SessionAgentConfig>,
) -> Result<LoopStats, CallerError> {
    // 1. Try to create presence provider. Degrade to silent mode on failure so
    //    an external-agent-only run (e.g. codex with no API keys configured)
    //    still starts. The main task loop below doesn't depend on the presence
    //    LLM — it only needs `task_rx` alive.
    let presence_provider_opt = match provider::select_presence_provider(
        project.config.presence.provider.as_deref(),
        project.config.presence.model.as_deref(),
    ) {
        Ok(p) => Some(p),
        Err(e) => {
            bus.send(AppEvent::PresenceLog {
                message: format!(
                    "Presence LLM unavailable ({}). Running without narration — \
                     dashboard chat and tasks will dispatch directly to the worker.",
                    e
                ),
                level: Some(types::LogLevel::Warn),
                turn: None,
            });
            None
        }
    };

    let fallback_task_tx = task_tx.clone();

    if let Some(presence_provider) = presence_provider_opt {
        bus.send(AppEvent::PresenceUsageUpdate {
            total_tokens: 0,
            context_window: project.config.presence.context_window,
            usage_pct: 0.0,
            provider: presence_provider.name().to_string(),
            model: presence_provider.model().to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
        });

        let presence_prompt = prompts::resolve_presence_prompt(Some(&project.root));
        let context_window = project.config.presence.context_window;
        let mut presence = presence::PresenceLayer::new(
            presence_provider,
            presence_prompt,
            context_window,
            bus.clone(),
            task_tx,
            presence_event_rx,
            agent_state.clone(),
            project.memory_path(),
            log_dir.clone(),
            project.root.clone(),
            presence_paused.clone(),
            context_injection.clone(),
        );

        // Send initial task to presence (if provided), with a timeout so a
        // slow or misconfigured presence provider doesn't freeze the TUI.
        let mut presence_failed_task: Option<String> = None;
        if let Some(ref task_str) = task {
            let input = format!("The user wants: {}", task_str);
            match tokio::time::timeout(
                tokio::time::Duration::from_secs(30),
                presence.process_user_input(&input),
            )
            .await
            {
                Ok(Ok(response)) if !response.is_empty() => {
                    let _ = response_tx.send(response).await;
                }
                Ok(Err(e)) => {
                    bus.send(AppEvent::PresenceLog {
                        message: format!(
                            "Presence provider error: {}. Use --no-presence or --direct to bypass. \
                             Submitting task directly.",
                            e
                        ),
                        level: Some(types::LogLevel::Warn),
                        turn: None,
                    });
                    presence_failed_task = Some(task_str.clone());
                }
                Err(_) => {
                    bus.send(AppEvent::PresenceLog {
                        message: "Presence provider timed out (30s). Use --no-presence or --direct to bypass. \
                             Submitting task directly."
                            .to_string(),
                        level: Some(types::LogLevel::Warn),
                        turn: None,
                    });
                    presence_failed_task = Some(task_str.clone());
                }
                _ => {}
            }
        }

        if let Some(failed_task) = presence_failed_task {
            let envelope = presence::TaskEnvelope {
                task: failed_task,
                force_direct: true,
                context_hints: vec![],
                reference_frame_ids: vec![],
                display_target: None,
                attachment_frame_ids: vec![],
                steer_id: None,
            };
            let _ = fallback_task_tx.send(envelope).await;
        }
        drop(fallback_task_tx);

        // Spawn presence.run() for user input + event narration.
        let _presence_handle = tokio::spawn(async move {
            presence.run(user_rx, response_tx).await;
        });
    } else {
        // Silent mode: no presence LLM. Inject the initial task directly and
        // forward subsequent user text from the dashboard chat into task_tx
        // as force_direct envelopes. presence_event_rx and response_tx are
        // dropped at scope exit — no consumer for them without a PresenceLayer.
        let _ = presence_event_rx;
        let _ = response_tx;
        let _ = agent_state;
        let _ = context_injection;

        if let Some(task_str) = task.as_ref() {
            let envelope = presence::TaskEnvelope {
                task: task_str.clone(),
                force_direct: true,
                context_hints: vec![],
                reference_frame_ids: vec![],
                display_target: None,
                attachment_frame_ids: vec![],
                steer_id: None,
            };
            let _ = fallback_task_tx.send(envelope).await;
        }
        // Keep task_tx alive for the forwarder below; drop the extra clone.
        drop(fallback_task_tx);

        let forwarder_tx = task_tx;
        let mut user_rx = user_rx;
        tokio::spawn(async move {
            while let Some(text) = user_rx.recv().await {
                let envelope = presence::TaskEnvelope {
                    task: text,
                    force_direct: true,
                    context_hints: vec![],
                    reference_frame_ids: vec![],
                    display_target: None,
                    attachment_frame_ids: vec![],
                    steer_id: None,
                };
                if forwarder_tx.send(envelope).await.is_err() {
                    break;
                }
            }
        });
    }

    // 8. Persistent server conversation across all presence tasks.
    //    First task initializes the conversation; subsequent tasks inject new
    //    user messages into the same conversation. This preserves the server
    //    model's context across the entire presence session.
    let mut cumulative_stats = LoopStats::default();
    let project_root = project.root.clone();

    // Resolve external agent backend: CLI override > web UI selection > config default > None.
    let initial_agent_backend = resolve_agent_backend_from_config(agent_backend_override, &project);
    // Seed the shared state so the web UI reflects the initial selection.
    {
        let mut guard = shared_external_agent.write().await;
        if guard.is_none() {
            *guard = initial_agent_backend.clone();
        }
    }

    // Conversation, provider, project — created on first task, reused thereafter.
    let mut persistent_conv: Option<Conversation> = None;
    let mut persistent_provider: Option<Box<dyn provider::ChatProvider>> = None;
    let mut persistent_project: Option<Project> = None;
    // External agent + thread — created on first task, reused for subsequent messages.
    let mut persistent_agent: Option<Box<dyn external_agent::ExternalAgent>> = None;
    let mut persistent_thread: Option<external_agent::AgentThread> = None;
    let mut persistent_event_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<external_agent::AgentEvent>,
    > = None;
    let mut persistent_diff_tracker = ExternalDiffDeltaTracker::default();
    let mut persistent_pending_runtime_steers: std::collections::VecDeque<PendingRuntimeSteer> =
        std::collections::VecDeque::new();
    let mut persistent_handled_steer_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut persistent_cancelled_follow_ups: HashSet<String> = HashSet::new();
    let mut persistent_open_side_threads: HashMap<String, String> = HashMap::new();
    let mut persistent_side_rounds: HashMap<String, usize> = HashMap::new();
    let mut persistent_side_turn_revisions: HashMap<String, UserTurnRevisionState> = HashMap::new();
    let mut persistent_pending_managed_context_replays: std::collections::VecDeque<
        FollowUpMessage,
    > = std::collections::VecDeque::new();
    let mut persistent_managed_context_recovery_kickstarts_without_rewind = 0u8;
    let mut persistent_managed_context_surgical_recoveries = 0u8;
    let mut startup_resume_session = resume_session;
    // Persisted per-session agent config for the startup resume, consumed by
    // the same agent build that consumes `startup_resume_session`.
    let mut startup_resume_session_config = resume_session_config;
    // Track which backend the persistent agent was created for, so we can reset
    // when the web UI changes the selection between tasks.
    let mut persistent_agent_backend: Option<external_agent::AgentBackend> = None;
    // Track the Codex runtime config the persistent agent was born with.
    // Codex locks sandbox / approval policy / model at `thread/start`, so
    // these can't change mid-thread — if any field differs from the current
    // `shared_codex_config` when a new task arrives, we tear the agent down
    // and build a fresh one. Only meaningful when the backend is Codex.
    let mut persistent_codex_config: Option<control_plane::CodexRuntimeConfig> = None;
    let mut persistent_claude_config: Option<control_plane::ClaudeRuntimeConfig> = None;

    // Side channel for thread actions (Codex slash commands) dispatched from
    // the dashboard / MCP between tasks. We subscribe to the bus here (not
    // just inside the drain) so actions still fire when the loop is idle,
    // waiting for the next task.
    let local_session_id = session_log_id(&session_log);
    let mut outer_bus_rx = bus.subscribe();
    // Turn controls (steer / interrupt) need to be subscribed before the
    // turn-start RPC. Otherwise an immediate follow-up can land during the
    // handoff and miss the running-turn drain entirely.
    let mut turn_bus_rx = bus.subscribe();
    let mut codex_thread_action_dedupe = CodexThreadActionDedupe::default();

    // Outer loop: either a task envelope arrives (run the agent), a thread
    // action arrives (invoke it on the persistent agent), or the task
    // channel closes (exit cleanly).
    enum OuterSignal {
        Task(presence::TaskEnvelope),
        ThreadAction {
            session_id: Option<String>,
            op: String,
            params: serde_json::Value,
        },
        /// Conversation-rollback request from the web gateway. Fired
        /// when the user POSTs `/api/session/current/rollback` with
        /// `revert_conversation: true`. The gateway only sends this
        /// when the agent is idle (guarded by `ensure_idle`), so
        /// handling it between tasks is safe.
        ConversationRollback {
            round_id: u64,
            target_native_message_count: Option<u32>,
            turns_to_drop: u32,
        },
        /// The persistent external agent produced an event while no task
        /// was being drained: an async sub-agent streaming between turns,
        /// or the backend starting a spontaneous turn (e.g. Claude Code's
        /// task-notification round after an async Agent-tool child ends).
        IdleAgentEvent(Box<external_agent::AgentEvent>),
        Done,
    }

    loop {
        let signal = tokio::select! {
            biased;
            env = task_rx.recv() => match env {
                Some(e) => OuterSignal::Task(e),
                None => OuterSignal::Done,
            },
            msg = outer_bus_rx.recv() => match msg {
                Ok(AppEvent::CodexThreadActionRequested {
                    request_id,
                    session_id,
                    action,
                    params,
                    ..
                }) if event_targets_external_session_or_side(
                    &session_id,
                    &local_session_id,
                    &persistent_thread.as_ref().map(|thread| thread.thread_id.clone()),
                    &persistent_open_side_threads,
                ) => {
                    if !codex_thread_action_dedupe.mark_seen(&request_id) {
                        continue;
                    }
                    OuterSignal::ThreadAction {
                        session_id,
                        op: action,
                        params,
                    }
                }
                Ok(AppEvent::ConversationRollbackRequested {
                    round_id,
                    target_native_message_count,
                    turns_to_drop,
                }) => OuterSignal::ConversationRollback {
                    round_id,
                    target_native_message_count,
                    turns_to_drop,
                },
                Ok(AppEvent::InterruptRequested { session_id })
                    if event_targets_session(&session_id, &local_session_id) =>
                {
                    // Drop idle interrupts so an old Stop action cannot
                    // interrupt the next task that happens to start later.
                    turn_bus_rx = bus.subscribe();
                    continue;
                }
                Ok(AppEvent::FollowUpCancelRequested {
                    session_id,
                    id,
                    reason,
                }) if event_targets_external_session_or_side(
                    &session_id,
                    &local_session_id,
                    &persistent_thread.as_ref().map(|thread| thread.thread_id.clone()),
                    &persistent_open_side_threads,
                ) => {
                    let status_session = session_id.as_deref().or(local_session_id.as_deref());
                    record_cancelled_follow_up_id(
                        &mut persistent_cancelled_follow_ups,
                        &bus,
                        status_session,
                        id,
                        &reason,
                    );
                    continue;
                }
                // Any other bus event: skip, keep selecting. Lagged /
                // Closed also fall through — task_rx close is the
                // authoritative "we're done" signal.
                _ => continue,
            },
            // Agent events while idle: without this arm they would buffer
            // until the next task's drain and complete it prematurely
            // (async Claude Code sub-agents finish — and the CLI starts its
            // notification turn — while the loop sits here).
            maybe_event = async {
                persistent_event_rx
                    .as_mut()
                    .expect("branch guarded by is_some")
                    .recv()
                    .await
            }, if persistent_event_rx.is_some() => match maybe_event {
                Some(event) => OuterSignal::IdleAgentEvent(Box::new(event)),
                None => {
                    // Reader task ended (agent process gone); disable the
                    // arm — the next task recreates the agent.
                    persistent_event_rx = None;
                    continue;
                }
            },
        };
        let envelope = match signal {
            OuterSignal::Task(e) => e,
            OuterSignal::Done => break,
            OuterSignal::ThreadAction {
                session_id,
                op,
                params,
            } => {
                let mut action_params = params;
                if let Some(request) = external_context_rewind_request_from_action(
                    &op,
                    &action_params,
                    session_id.clone(),
                ) {
                    let request = match request {
                        Ok(request) => request,
                        Err(message) => {
                            bus.send(AppEvent::CodexThreadActionResult {
                                session_id: session_id.clone().or_else(|| local_session_id.clone()),
                                action: op,
                                success: false,
                                message,
                                record_id: None,
                            });
                            turn_bus_rx = bus.subscribe();
                            continue;
                        }
                    };
                    let Some(ref mut agent) = persistent_agent else {
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id: session_id.clone().or_else(|| local_session_id.clone()),
                            action: op,
                            success: false,
                            message: "no active agent — start a task first".to_string(),
                            record_id: None,
                        });
                        turn_bus_rx = bus.subscribe();
                        continue;
                    };
                    let Some(thread) = persistent_thread.as_ref() else {
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id: session_id.clone().or_else(|| local_session_id.clone()),
                            action: op,
                            success: false,
                            message: "no active Codex thread — start a task first".to_string(),
                            record_id: None,
                        });
                        turn_bus_rx = bus.subscribe();
                        continue;
                    };
                    let drain_config = DrainConfig {
                        bus: &bus,
                        web_port,
                        session_id: session_log_id(&session_log),
                        alias_session_id: Some(thread.thread_id.clone()),
                        backend_thread_id: Some(thread.thread_id.clone()),
                        autonomy: autonomy.clone(),
                        session_log: &session_log,
                        project_root: &project.root,
                        log_dir: &log_dir,
                        approval_registry: &approval_registry,
                        json_approval: None,
                        agent_source: Some("Codex".to_string()),
                        suppress_agent_started: true,
                        persist_model_responses_inline: false,
                        headless: false,
                        context_injection: &context_injection,
                    };
                    match apply_external_context_rewind(
                        agent,
                        &thread.thread_id,
                        &request,
                        &drain_config,
                    )
                    .await
                    {
                        Ok(Some(followup)) => {
                            if let Some(event_rx) = persistent_event_rx.as_mut() {
                                let mut side_session_state = ExternalSideSessionState {
                                    open_side_threads: &mut persistent_open_side_threads,
                                    side_rounds: &mut persistent_side_rounds,
                                    side_turn_revisions: &mut persistent_side_turn_revisions,
                                };
                                let mut resume = ExternalContextRewindResume {
                                    event_rx,
                                    turn_bus_rx: &mut turn_bus_rx,
                                    config: &drain_config,
                                    stats: &mut cumulative_stats,
                                    diff_tracker: &mut persistent_diff_tracker,
                                    pending_runtime_steers: &mut persistent_pending_runtime_steers,
                                    handled_steer_ids: &mut persistent_handled_steer_ids,
                                    cancelled_follow_ups: &mut persistent_cancelled_follow_ups,
                                    codex_thread_action_dedupe: &mut codex_thread_action_dedupe,
                                    side_sessions: Some(&mut side_session_state),
                                };
                                match send_external_context_rewind_resume_turn(
                                    agent,
                                    thread,
                                    followup,
                                    &mut resume,
                                )
                                .await
                                {
                                    Ok(DrainOutcome::TurnCompleted {
                                        message,
                                        turns_in_round,
                                    }) => {
                                        cumulative_stats.turns += 1;
                                        cumulative_stats.rounds += 1;
                                        bus.send(AppEvent::DoneSignal {
                                            session_id: session_log_id(&session_log),
                                            message: message.clone(),
                                        });
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                    }
                                    Ok(DrainOutcome::ContextRewindRequested {
                                        request, ..
                                    }) => {
                                        match apply_chained_context_rewind_resume_turns(
                                            agent,
                                            thread,
                                            request,
                                            &mut resume,
                                        )
                                        .await
                                        {
                                            Ok(Some(DrainOutcome::TurnCompleted {
                                                message,
                                                turns_in_round,
                                            })) => {
                                                cumulative_stats.turns += 1;
                                                cumulative_stats.rounds += 1;
                                                bus.send(AppEvent::DoneSignal {
                                                    session_id: session_log_id(&session_log),
                                                    message: message.clone(),
                                                });
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: session_log_id(&session_log),
                                                    round: cumulative_stats.rounds,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                });
                                            }
                                            Ok(Some(DrainOutcome::RecoveryRequired {
                                                message,
                                                recovery_hint,
                                                turns_in_round,
                                            })) => {
                                                cumulative_stats.rounds += 1;
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: session_log_id(&session_log),
                                                    round: cumulative_stats.rounds,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                });
                                                bus.send(AppEvent::PresenceLog {
                                                    message: recovery_required_message(
                                                        &message,
                                                        recovery_hint.as_deref(),
                                                    ),
                                                    level: Some(types::LogLevel::Warn),
                                                    turn: None,
                                                });
                                            }
                                            Ok(Some(DrainOutcome::Interrupted { reason })) => {
                                                bus.send(AppEvent::PresenceLog {
                                                    message: format!(
                                                        "External agent interrupted during resumed context-rewind turn: {}",
                                                        reason
                                                    ),
                                                    level: None,
                                                    turn: None,
                                                });
                                            }
                                            Ok(Some(DrainOutcome::Terminated {
                                                reason, ..
                                            })) => {
                                                bus.send(AppEvent::PresenceLog {
                                                    message: format!(
                                                        "External agent terminated: {}",
                                                        reason
                                                    ),
                                                    level: Some(types::LogLevel::Error),
                                                    turn: None,
                                                });
                                                persistent_agent = None;
                                                persistent_thread = None;
                                                persistent_event_rx = None;
                                                persistent_diff_tracker =
                                                    ExternalDiffDeltaTracker::default();
                                                persistent_pending_runtime_steers.clear();
                                                persistent_handled_steer_ids.clear();
                                                persistent_open_side_threads.clear();
                                                persistent_side_rounds.clear();
                                                persistent_side_turn_revisions.clear();
                                            }
                                            Ok(Some(DrainOutcome::ChannelClosed)) => {
                                                persistent_agent = None;
                                                persistent_thread = None;
                                                persistent_event_rx = None;
                                                persistent_diff_tracker =
                                                    ExternalDiffDeltaTracker::default();
                                                persistent_pending_runtime_steers.clear();
                                                persistent_handled_steer_ids.clear();
                                                persistent_open_side_threads.clear();
                                                persistent_side_rounds.clear();
                                                persistent_side_turn_revisions.clear();
                                            }
                                            Ok(Some(DrainOutcome::ContextRewindRequested {
                                                request,
                                                ..
                                            })) => {
                                                emit_context_rewind_failure(
                                                    &request,
                                                    "chained context rewind returned an unexpected pending rewind"
                                                        .to_string(),
                                                    &drain_config,
                                                );
                                            }
                                            Ok(None) => {}
                                            Err((request, message)) => {
                                                emit_context_rewind_failure(
                                                    &request,
                                                    message,
                                                    &drain_config,
                                                );
                                            }
                                        }
                                    }
                                    Ok(DrainOutcome::RecoveryRequired {
                                        message,
                                        recovery_hint,
                                        turns_in_round,
                                    }) => {
                                        cumulative_stats.rounds += 1;
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        bus.send(AppEvent::PresenceLog {
                                            message: recovery_required_message(
                                                &message,
                                                recovery_hint.as_deref(),
                                            ),
                                            level: Some(types::LogLevel::Warn),
                                            turn: None,
                                        });
                                    }
                                    Ok(DrainOutcome::Interrupted { reason }) => {
                                        bus.send(AppEvent::PresenceLog {
                                            message: format!(
                                                "External agent interrupted during resumed context-rewind turn: {}",
                                                reason
                                            ),
                                            level: None,
                                            turn: None,
                                        });
                                    }
                                    Ok(DrainOutcome::Terminated { reason, .. }) => {
                                        bus.send(AppEvent::PresenceLog {
                                            message: format!(
                                                "External agent terminated: {}",
                                                reason
                                            ),
                                            level: Some(types::LogLevel::Error),
                                            turn: None,
                                        });
                                        persistent_agent = None;
                                        persistent_thread = None;
                                        persistent_event_rx = None;
                                        persistent_diff_tracker =
                                            ExternalDiffDeltaTracker::default();
                                        persistent_pending_runtime_steers.clear();
                                        persistent_handled_steer_ids.clear();
                                        persistent_open_side_threads.clear();
                                        persistent_side_rounds.clear();
                                        persistent_side_turn_revisions.clear();
                                    }
                                    Ok(DrainOutcome::ChannelClosed) => {
                                        persistent_agent = None;
                                        persistent_thread = None;
                                        persistent_event_rx = None;
                                        persistent_diff_tracker =
                                            ExternalDiffDeltaTracker::default();
                                        persistent_pending_runtime_steers.clear();
                                        persistent_handled_steer_ids.clear();
                                        persistent_open_side_threads.clear();
                                        persistent_side_rounds.clear();
                                        persistent_side_turn_revisions.clear();
                                    }
                                    Err(message) => {
                                        emit_context_rewind_failure(
                                            &request,
                                            message,
                                            &drain_config,
                                        );
                                    }
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(message) => {
                            emit_context_rewind_failure(&request, message, &drain_config);
                        }
                    }
                    turn_bus_rx = bus.subscribe();
                    continue;
                }
                // `/new` is a daemon-side operation (not a Codex RPC): clear
                // the persistent agent so the next task creates a fresh
                // thread. Handled here — not inside dispatch_thread_action
                // — because the Box<dyn ExternalAgent> lives in this loop.
                let result = if op == "new" {
                    persistent_agent = None;
                    persistent_thread = None;
                    persistent_event_rx = None;
                    persistent_codex_config = None;
                    persistent_diff_tracker = ExternalDiffDeltaTracker::default();
                    persistent_open_side_threads.clear();
                    persistent_side_rounds.clear();
                    persistent_side_turn_revisions.clear();
                    Ok("agent torn down; next task will start a fresh thread".to_string())
                } else if is_context_rewind_anchor_list_action(&op)
                    || is_context_rewind_anchor_inspect_action(&op)
                {
                    let Some(ref mut agent) = persistent_agent else {
                        turn_bus_rx = bus.subscribe();
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id: session_id.clone().or_else(|| local_session_id.clone()),
                            action: op,
                            success: false,
                            message: "no active agent — start a task first".to_string(),
                            record_id: None,
                        });
                        continue;
                    };
                    let target_thread_id = session_id
                        .as_deref()
                        .filter(|id| Some(*id) != local_session_id.as_deref())
                        .or_else(|| {
                            persistent_thread
                                .as_ref()
                                .map(|thread| thread.thread_id.as_str())
                        });
                    action_params =
                        thread_action_params_with_thread_id(&op, action_params, target_thread_id);
                    if is_context_rewind_anchor_list_action(&op) {
                        apply_context_rewind_anchor_list_action(agent, &action_params).await
                    } else {
                        apply_context_rewind_anchor_inspect_action(agent, &action_params).await
                    }
                } else if is_context_rewind_backout_action(&op) {
                    let Some(ref mut agent) = persistent_agent else {
                        turn_bus_rx = bus.subscribe();
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id: session_id.clone().or_else(|| local_session_id.clone()),
                            action: op,
                            success: false,
                            message: "no active agent — start a task first".to_string(),
                            record_id: None,
                        });
                        continue;
                    };
                    let target_thread_id = session_id
                        .as_deref()
                        .filter(|id| Some(*id) != local_session_id.as_deref())
                        .or_else(|| {
                            persistent_thread
                                .as_ref()
                                .map(|thread| thread.thread_id.as_str())
                        });
                    action_params =
                        thread_action_params_with_thread_id(&op, action_params, target_thread_id);
                    let drain_config = DrainConfig {
                        bus: &bus,
                        web_port,
                        session_id: session_log_id(&session_log),
                        alias_session_id: persistent_thread
                            .as_ref()
                            .map(|thread| thread.thread_id.clone()),
                        backend_thread_id: persistent_thread
                            .as_ref()
                            .map(|thread| thread.thread_id.clone()),
                        autonomy: autonomy.clone(),
                        session_log: &session_log,
                        project_root: &project.root,
                        log_dir: &log_dir,
                        approval_registry: &approval_registry,
                        json_approval: None,
                        agent_source: Some("Codex".to_string()),
                        suppress_agent_started: true,
                        persist_model_responses_inline: false,
                        headless: false,
                        context_injection: &context_injection,
                    };
                    apply_context_rewind_backout_action(agent, &op, &action_params, &drain_config)
                        .await
                } else if is_fission_spawn_action(&op) || is_fission_import_action(&op) {
                    let Some(ref mut agent) = persistent_agent else {
                        turn_bus_rx = bus.subscribe();
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id: session_id.clone().or_else(|| local_session_id.clone()),
                            action: op,
                            success: false,
                            message: "no active agent — start a task first".to_string(),
                            record_id: None,
                        });
                        continue;
                    };
                    let target_thread_id = session_id
                        .as_deref()
                        .filter(|id| Some(*id) != local_session_id.as_deref())
                        .or_else(|| {
                            persistent_thread
                                .as_ref()
                                .map(|thread| thread.thread_id.as_str())
                        });
                    action_params =
                        thread_action_params_with_thread_id(&op, action_params, target_thread_id);
                    let drain_config = DrainConfig {
                        bus: &bus,
                        web_port,
                        session_id: session_log_id(&session_log),
                        alias_session_id: persistent_thread
                            .as_ref()
                            .map(|thread| thread.thread_id.clone()),
                        backend_thread_id: persistent_thread
                            .as_ref()
                            .map(|thread| thread.thread_id.clone()),
                        autonomy: autonomy.clone(),
                        session_log: &session_log,
                        project_root: &project.root,
                        log_dir: &log_dir,
                        approval_registry: &approval_registry,
                        json_approval: None,
                        agent_source: Some("Codex".to_string()),
                        suppress_agent_started: true,
                        persist_model_responses_inline: false,
                        headless: false,
                        context_injection: &context_injection,
                    };
                    if is_fission_spawn_action(&op) {
                        apply_fission_spawn_action(agent, &action_params, &drain_config).await
                    } else {
                        apply_fission_import_action(agent, &action_params, &drain_config).await
                    }
                } else if let Some(ref mut agent) = persistent_agent {
                    // Backends without an in-process fork (Claude Code) fork
                    // by respawning — mirror the drain-level
                    // `ForkHandling::RespawnResume` branch. (This inline
                    // presence dispatcher duplicates the drain's action
                    // handling; keep the two in sync.)
                    if op == "fork" {
                        if let external_agent::ForkHandling::RespawnResume { thread_id } =
                            agent.fork_handling()
                        {
                            let (success, message) = match thread_id {
                                Some(parent_thread_id) => {
                                    bus.send(AppEvent::ControlCommand(
                                        event::ControlMsg::ResumeSession {
                                            source: agent.name().to_string(),
                                            session_id: parent_thread_id.clone(),
                                            resume_id: Some(parent_thread_id.clone()),
                                            project_root: Some(
                                                project.root.to_string_lossy().to_string(),
                                            ),
                                            task: None,
                                            direct: Some(true),
                                            attachments: Vec::new(),
                                            fork: true,
                                            agent_command:
                                                crate::session_config::read_log_dir_config(
                                                    &log_dir,
                                                )
                                                .and_then(|cfg| cfg.agent_command),
                                            codex_sandbox: None,
                                            codex_approval_policy: None,
                                            codex_managed_context: None,
                                            codex_context_archive: None,
                                        },
                                    ));
                                    (
                                        true,
                                        format!(
                                            "forking thread {} — the fork announces its own session id on its first turn",
                                            short_external_session_id(&parent_thread_id)
                                        ),
                                    )
                                }
                                None => (
                                    false,
                                    "fork needs a native session id — run a turn in this session first"
                                        .to_string(),
                                ),
                            };
                            slog(&session_log, |l| {
                                l.info(&format!(
                                    "{} thread action /fork: {} — {}",
                                    agent.name(),
                                    if success { "ok" } else { "FAILED" },
                                    message
                                ))
                            });
                            bus.send(AppEvent::CodexThreadActionResult {
                                session_id: session_id.or_else(|| local_session_id.clone()),
                                action: op,
                                success,
                                message,
                                record_id: None,
                            });
                            continue;
                        }
                    }
                    let target_thread_id = session_id
                        .as_deref()
                        .filter(|id| Some(*id) != local_session_id.as_deref())
                        .or_else(|| {
                            persistent_thread
                                .as_ref()
                                .map(|thread| thread.thread_id.as_str())
                        });
                    action_params =
                        thread_action_params_with_thread_id(&op, action_params, target_thread_id);
                    agent
                        .thread_action(&op, &action_params)
                        .await
                        .map_err(|e| e.to_string())
                } else {
                    Err("no active agent — start a task first".to_string())
                };
                let (success, message) = match result {
                    Ok(msg) => (true, msg),
                    Err(e) => (false, e),
                };
                let result_session_id = session_id.or_else(|| local_session_id.clone());
                slog(&session_log, |l| {
                    l.info(&format!(
                        "Codex thread action /{}: {} — {}",
                        op,
                        if success { "ok" } else { "FAILED" },
                        codex_thread_action_log_message(&op, &message)
                    ))
                });
                bus.send(AppEvent::CodexThreadActionResult {
                    session_id: result_session_id.clone(),
                    action: op.clone(),
                    success,
                    message: message.clone(),
                    record_id: None,
                });
                if success && op == "fast" {
                    let service_tier = persistent_agent
                        .as_ref()
                        .and_then(|agent| agent.service_tier().map(str::to_string));
                    let drain_config = DrainConfig {
                        bus: &bus,
                        web_port,
                        session_id: local_session_id.clone(),
                        alias_session_id: persistent_thread
                            .as_ref()
                            .map(|thread| thread.thread_id.clone()),
                        backend_thread_id: persistent_thread
                            .as_ref()
                            .map(|thread| thread.thread_id.clone()),
                        autonomy: autonomy.clone(),
                        session_log: &session_log,
                        project_root: &project.root,
                        log_dir: &log_dir,
                        approval_registry: &approval_registry,
                        json_approval: None,
                        agent_source: Some("Codex".to_string()),
                        suppress_agent_started: true,
                        persist_model_responses_inline: false,
                        headless: false,
                        context_injection: &context_injection,
                    };
                    persist_codex_service_tier_for_drain(
                        &drain_config,
                        result_session_id.as_deref(),
                        service_tier.as_deref(),
                    );
                    emit_codex_session_capabilities_for_drain(
                        &drain_config,
                        result_session_id.as_deref(),
                        service_tier.as_deref(),
                    );
                }
                if success && op == "fork" {
                    if let Some(child_id) = forked_thread_id_from_message(&message) {
                        emit_codex_fork_session_name(&bus, &child_id, &action_params);
                        emit_session_relationship(
                            &bus,
                            result_session_id.as_deref(),
                            &child_id,
                            "fork",
                            false,
                        );
                        bus.send(AppEvent::ControlCommand(event::ControlMsg::ResumeSession {
                            source: "codex".to_string(),
                            session_id: child_id.clone(),
                            resume_id: Some(child_id),
                            project_root: Some(project_root.to_string_lossy().to_string()),
                            task: None,
                            direct: Some(true),
                            fork: false,
                            attachments: Vec::new(),
                            agent_command: Some(project.config.agent.codex.command.clone()),
                            codex_sandbox: Some(crate::project::normalize_sandbox_mode(
                                &project.config.agent.codex.sandbox,
                            )),
                            codex_approval_policy: Some(crate::project::normalize_approval_policy(
                                &project.config.agent.codex.approval_policy,
                            )),
                            codex_managed_context: Some(
                                crate::project::normalize_codex_managed_context(
                                    &project.config.agent.codex.managed_context,
                                ),
                            ),
                            codex_context_archive: Some(
                                crate::project::normalize_codex_context_archive(
                                    &project.config.agent.codex.context_archive,
                                ),
                            ),
                        }));
                    }
                }
                if success && op == "side" {
                    if let Some((parent_thread_id, child_thread_id)) =
                        side_thread_ids_from_message(&message)
                    {
                        let side_prompt = side_session_prompt_from_params(&action_params);
                        {
                            let mut side_state = ExternalSideSessionState {
                                open_side_threads: &mut persistent_open_side_threads,
                                side_rounds: &mut persistent_side_rounds,
                                side_turn_revisions: &mut persistent_side_turn_revisions,
                            };
                            side_state
                                .record_started(parent_thread_id.clone(), child_thread_id.clone());
                        }
                        if let (Some(agent), Some(event_rx)) =
                            (persistent_agent.as_mut(), persistent_event_rx.as_mut())
                        {
                            let drain_config = DrainConfig {
                                bus: &bus,
                                web_port,
                                session_id: session_log_id(&session_log),
                                alias_session_id: None,
                                backend_thread_id: persistent_thread
                                    .as_ref()
                                    .map(|thread| thread.thread_id.clone()),
                                autonomy: autonomy.clone(),
                                session_log: &session_log,
                                project_root: &project.root,
                                log_dir: &log_dir,
                                approval_registry: &approval_registry,
                                json_approval: None,
                                agent_source: Some("Codex".to_string()),
                                suppress_agent_started: true,
                                persist_model_responses_inline: false,
                                headless: false,
                                context_injection: &context_injection,
                            };
                            emit_side_session_started(
                                &drain_config,
                                &parent_thread_id,
                                &child_thread_id,
                                side_prompt.as_deref(),
                            );
                            // `turn_bus_rx` was subscribed before the
                            // `/side` request was broadcast, so it may still
                            // contain the triggering CodexThreadActionRequested
                            // event. Use a fresh receiver for the child drain
                            // to avoid dispatching `/side` a second time.
                            let mut side_bus_rx = bus.subscribe();
                            drain_external_child_turn(
                                agent,
                                event_rx,
                                &mut side_bus_rx,
                                &drain_config,
                                &mut cumulative_stats,
                                &mut persistent_diff_tracker,
                                &mut persistent_pending_runtime_steers,
                                &mut persistent_handled_steer_ids,
                                &mut persistent_cancelled_follow_ups,
                                &mut codex_thread_action_dedupe,
                                child_thread_id,
                                "side",
                            )
                            .await;
                        } else {
                            slog(&session_log, |l| {
                                l.warn("Codex side conversation started but no event receiver is available")
                            });
                        }
                    }
                } else if success && matches!(op.as_str(), "side-close" | "side_close") {
                    if let Some(child_thread_id) = side_child_thread_id_from_params(&action_params)
                    {
                        let mut side_state = ExternalSideSessionState {
                            open_side_threads: &mut persistent_open_side_threads,
                            side_rounds: &mut persistent_side_rounds,
                            side_turn_revisions: &mut persistent_side_turn_revisions,
                        };
                        side_state.record_closed(&child_thread_id);
                    }
                }
                turn_bus_rx = bus.subscribe();
                continue;
            }
            OuterSignal::IdleAgentEvent(event) => {
                // Inline mirror of run_external_agent_mode's idle listener
                // (the drain loops there and here must stay in sync): route
                // sub-agent-scoped events to their child windows, absorb
                // identity/goal/termination housekeeping, and treat any
                // other primary event as the start of a spontaneous backend
                // round drained to completion.
                let (event_thread_id, event_turn_id, event) = event.into_scope();
                let persistent_thread_id = persistent_thread
                    .as_ref()
                    .map(|thread| thread.thread_id.clone());
                let idle_drain_config = DrainConfig {
                    bus: &bus,
                    web_port,
                    session_id: session_log_id(&session_log),
                    alias_session_id: persistent_thread_id.clone(),
                    backend_thread_id: persistent_thread_id.clone(),
                    autonomy: autonomy.clone(),
                    session_log: &session_log,
                    project_root: &project.root,
                    log_dir: &log_dir,
                    approval_registry: &approval_registry,
                    json_approval: None,
                    agent_source: Some(
                        persistent_agent_backend
                            .as_ref()
                            .map(|backend| backend.to_string())
                            .unwrap_or_else(|| "Codex".to_string()),
                    ),
                    suppress_agent_started: true,
                    persist_model_responses_inline: false,
                    headless: false,
                    context_injection: &context_injection,
                };
                if let Some(child_thread_id) =
                    scoped_event_codex_subagent_thread_id(&event_thread_id, &cumulative_stats)
                {
                    handle_idle_codex_subagent_event(
                        &idle_drain_config,
                        &mut cumulative_stats,
                        child_thread_id,
                        event,
                    );
                    continue;
                }
                match event {
                    external_agent::AgentEvent::NativeSessionId { session_id } => {
                        persist_native_backend_session_id(&idle_drain_config, &session_id);
                        let is_canonical = persistent_agent_backend
                            .as_ref()
                            .is_some_and(|backend| backend.thread_id_is_canonical(&session_id));
                        if is_canonical {
                            if let Some(thread) = persistent_thread.as_mut() {
                                thread.thread_id = session_id;
                            }
                        }
                    }
                    external_agent::AgentEvent::GoalUpdated { goal } => {
                        emit_external_session_goal(
                            &idle_drain_config,
                            event_thread_id,
                            Some(goal),
                        );
                    }
                    external_agent::AgentEvent::GoalCleared => {
                        emit_external_session_goal(&idle_drain_config, event_thread_id, None);
                    }
                    // Passive housekeeping renders directly — it must NOT
                    // open a spontaneous round. A lone log (e.g. "Compacting
                    // context…" from an idle /compact whose free result the
                    // adapter absorbs) would otherwise open a round that
                    // nothing ever completes, wedging the loop while queued
                    // tasks rot in task_rx.
                    external_agent::AgentEvent::Log { level, message } => {
                        bus.send(AppEvent::LogEntry {
                            session_id: session_log_id(&session_log),
                            level,
                            source: external_agent_log_source(
                                idle_drain_config.agent_source.as_deref(),
                            ),
                            content: message,
                            turn: None,
                        });
                    }
                    external_agent::AgentEvent::Usage { usage } => {
                        bus.send(AppEvent::UsageSnapshot {
                            session_id: session_log_id(&session_log),
                            main: frontend::ModelUsageSnapshot {
                                provider: usage.provider,
                                model: usage.model,
                                tokens_used: usage.tokens_used,
                                context_window: usage.context_window,
                                hard_context_window: usage.hard_context_window,
                                usage_pct: usage.usage_pct,
                                prompt_tokens: usage.prompt_tokens,
                                completion_tokens: usage.completion_tokens,
                                cached_tokens: usage.cached_tokens,
                            },
                            presence: None,
                        });
                    }
                    external_agent::AgentEvent::BackendError {
                        message,
                        code,
                        details,
                        will_retry,
                        ..
                    } => {
                        let label = external_agent_log_source(
                            idle_drain_config.agent_source.as_deref(),
                        );
                        let mut content = if let Some(code) = code.as_deref() {
                            format!("{label} backend error while idle ({code}): {message}")
                        } else {
                            format!("{label} backend error while idle: {message}")
                        };
                        if let Some(details) =
                            details.as_deref().filter(|s| !s.trim().is_empty())
                        {
                            content.push('\n');
                            content.push_str(details.trim());
                        }
                        slog(&session_log, |l| {
                            if will_retry {
                                l.warn(&content)
                            } else {
                                l.error(&content)
                            }
                        });
                        bus.send(AppEvent::LogEntry {
                            session_id: session_log_id(&session_log),
                            level: if will_retry { "warn" } else { "error" }.to_string(),
                            source: label,
                            content,
                            turn: None,
                        });
                    }
                    external_agent::AgentEvent::Terminated { reason, exit_code } => {
                        slog(&session_log, |l| {
                            l.warn(&format!(
                                "External agent terminated while idle: {} (exit code: {:?})",
                                reason, exit_code
                            ))
                        });
                        bus.send(AppEvent::PresenceLog {
                            message: format!("External agent terminated while idle: {reason}"),
                            level: Some(types::LogLevel::Warn),
                            turn: None,
                        });
                        persistent_agent = None;
                        persistent_thread = None;
                        persistent_event_rx = None;
                    }
                    other => {
                        let targets_primary = scoped_event_targets_config(
                            &event_thread_id,
                            &local_session_id,
                            &persistent_thread_id,
                        );
                        let targets_side = event_thread_id
                            .as_deref()
                            .is_some_and(|id| persistent_open_side_threads.contains_key(id));
                        if !targets_primary && !targets_side {
                            continue;
                        }
                        if let (Some(agent), Some(event_rx)) =
                            (persistent_agent.as_mut(), persistent_event_rx.as_mut())
                        {
                            let round = cumulative_stats.rounds.saturating_add(1);
                            emit_external_turn_status(
                                &bus,
                                &autonomy,
                                session_log_id(&session_log).as_deref(),
                                round,
                                "running",
                                format!(
                                    "{} backend turn {} observed while idle",
                                    agent.name(),
                                    round
                                ),
                            )
                            .await;
                            let mut prefetched_events = std::collections::VecDeque::new();
                            prefetched_events.push_back(external_agent::AgentEvent::scoped(
                                event_thread_id,
                                event_turn_id,
                                other,
                            ));
                            let mut side_session_state = ExternalSideSessionState {
                                open_side_threads: &mut persistent_open_side_threads,
                                side_rounds: &mut persistent_side_rounds,
                                side_turn_revisions: &mut persistent_side_turn_revisions,
                            };
                            let outcome = drain_external_agent_events_with_prefetched(
                                agent,
                                event_rx,
                                &mut turn_bus_rx,
                                &idle_drain_config,
                                &mut cumulative_stats,
                                &mut persistent_diff_tracker,
                                &mut persistent_pending_runtime_steers,
                                &mut persistent_handled_steer_ids,
                                &mut persistent_cancelled_follow_ups,
                                &mut codex_thread_action_dedupe,
                                &mut prefetched_events,
                                Some(&mut side_session_state),
                                false,
                                false,
                                false,
                            )
                            .await;
                            if let Some(native) =
                                cumulative_stats.announced_native_session_id.take()
                            {
                                let is_canonical =
                                    persistent_agent_backend.as_ref().is_some_and(|backend| {
                                        backend.thread_id_is_canonical(&native)
                                    });
                                if is_canonical {
                                    if let Some(thread) = persistent_thread.as_mut() {
                                        if thread.thread_id != native {
                                            thread.thread_id = native;
                                        }
                                    }
                                }
                            }
                            match outcome {
                                DrainOutcome::TurnCompleted {
                                    message,
                                    turns_in_round,
                                } => {
                                    cumulative_stats.turns += 1;
                                    cumulative_stats.rounds = round;
                                    bus.send(AppEvent::DoneSignal {
                                        session_id: session_log_id(&session_log),
                                        message: message.clone(),
                                    });
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: session_log_id(&session_log),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                }
                                DrainOutcome::Interrupted { .. } => {
                                    cumulative_stats.rounds = round;
                                }
                                DrainOutcome::RecoveryRequired {
                                    message,
                                    turns_in_round,
                                    ..
                                } => {
                                    cumulative_stats.rounds = round;
                                    slog(&session_log, |l| {
                                        l.warn(&format!(
                                            "Spontaneous external round ended in recovery state: {message}"
                                        ))
                                    });
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: session_log_id(&session_log),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                }
                                DrainOutcome::ContextRewindRequested {
                                    message,
                                    turns_in_round,
                                    ..
                                } => {
                                    // Rewinds are requested by managed Codex
                                    // turns; a spontaneous round has no task
                                    // to resume into, so complete the round
                                    // and drop the request.
                                    cumulative_stats.rounds = round;
                                    slog(&session_log, |l| {
                                        l.warn(
                                            "Dropping context-rewind request from a spontaneous external round",
                                        )
                                    });
                                    bus.send(AppEvent::DoneSignal {
                                        session_id: session_log_id(&session_log),
                                        message: message.clone(),
                                    });
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: session_log_id(&session_log),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                }
                                DrainOutcome::Terminated { reason, .. } => {
                                    slog(&session_log, |l| {
                                        l.warn(&format!(
                                            "External agent terminated during spontaneous round: {reason}"
                                        ))
                                    });
                                    persistent_agent = None;
                                    persistent_thread = None;
                                    persistent_event_rx = None;
                                }
                                DrainOutcome::ChannelClosed => {
                                    persistent_agent = None;
                                    persistent_thread = None;
                                    persistent_event_rx = None;
                                }
                            }
                        }
                    }
                }
                continue;
            }
            OuterSignal::ConversationRollback {
                round_id,
                target_native_message_count,
                turns_to_drop,
            } => {
                // Three possible states:
                //   1. External agent active (Codex / CC / Gemini)
                //   2. Native agent active (persistent_conv is Some)
                //   3. Neither — nothing to roll back from
                //
                // For external agents we try `rollback_turns` first; on
                // the default "not supported" error we fall back to a
                // session reset (shut down, clear persistent state; the
                // next task will re-initialize from scratch).
                if let Some(ref mut agent) = persistent_agent {
                    let backend_name = agent.name().to_ascii_lowercase().replace(' ', "-");
                    match agent.rollback_turns(turns_to_drop).await {
                        Ok(()) => {
                            bus.send(AppEvent::ConversationRolledBack {
                                round_id,
                                turns_removed: turns_to_drop,
                                backend: backend_name,
                                method: "truncated".into(),
                            });
                        }
                        Err(e) => {
                            // Fall back to a session reset: shut the
                            // agent down, drop persistent handles, and
                            // let the next task re-initialize. This
                            // loses conversation context — the only
                            // honest behavior when the protocol doesn't
                            // expose rollback.
                            slog(&session_log, |l| {
                                l.warn(&format!(
                                    "Conversation rollback via protocol failed ({}); falling back to session reset",
                                    e
                                ))
                            });
                            let _ = agent.shutdown().await;
                            persistent_agent = None;
                            persistent_thread = None;
                            persistent_event_rx = None;
                            persistent_codex_config = None;
                            persistent_claude_config = None;
                            bus.send(AppEvent::ConversationRolledBack {
                                round_id,
                                turns_removed: turns_to_drop,
                                backend: backend_name,
                                method: "session-reset".into(),
                            });
                        }
                    }
                } else if let Some(ref mut conv) = persistent_conv {
                    // Native path: truncate the messages array to the
                    // recorded length. If the round didn't store a
                    // native_message_count (e.g. an external-agent
                    // round), we can't truncate meaningfully; log and
                    // emit a 0-turn event so the dashboard clears the
                    // pending state.
                    let removed = match target_native_message_count {
                        Some(n) => conv.truncate_to(n as usize),
                        None => 0,
                    };
                    bus.send(AppEvent::ConversationRolledBack {
                        round_id,
                        turns_removed: removed as u32,
                        backend: "native".into(),
                        method: "truncated".into(),
                    });
                } else {
                    // No conversation to revert — emit completion
                    // anyway so the dashboard doesn't wait forever.
                    bus.send(AppEvent::ConversationRolledBack {
                        round_id,
                        turns_removed: 0,
                        backend: "native".into(),
                        method: "truncated".into(),
                    });
                }
                turn_bus_rx = bus.subscribe();
                continue;
            }
        };
        // Backend-side dispatch log: emitted at task acceptance, replacing the
        // legacy TUI-side log so headless and dashboard-direct tasks both reach
        // external consumers regardless of which frontend is running.
        emit_task_dispatched_log(
            &bus,
            &session_log,
            &envelope.task,
            envelope.attachment_frame_ids.len(),
        );

        // Pause server-side presence narration for direct-mode tasks — no
        // narration, no hallucinated side-tasks, no 400 errors from Gemini.
        // Programmatic clients (WebSocket with direct:true) don't need it.
        // Uses fetch_add/fetch_sub so it composes with browser voice's
        // ref-count (PresenceConnected += 1, PresenceDisconnected -= 1) —
        // each pause source is one independent reason to mute narration.
        let _direct_pause = if envelope.force_direct {
            Some(PresencePauseGuard::new(presence_paused.clone()))
        } else {
            None
        };

        slog(&session_log, |l| {
            l.debug(&format!(
                "{}task: {}",
                if envelope.force_direct {
                    "Direct "
                } else {
                    "Presence dispatched "
                },
                envelope.task
            ));
        });

        // Resolve frame context_hints → images
        let frame_images = resolve_frame_hints(&envelope.context_hints, &frame_registry).await;

        // Resolve user-attached frames → images. These come from the dashboard's
        // "Attach" buttons (annotation toolbar / Video tab) and are appended to
        // the first user message of the agent conversation, in addition to
        // anything from `context_hints`.
        let attachment_images =
            resolve_frame_ids(&envelope.attachment_frame_ids, &frame_registry).await;
        if !attachment_images.is_empty() {
            slog(&session_log, |l| {
                l.debug(&format!(
                    "Task has {} user attachment(s)",
                    attachment_images.len()
                ))
            });
        }

        // ── CU-first routing (VAULTED — [experimental] cu_first_routing) ──
        // When enabled, every non-direct task is intercepted by a fast CU
        // model that either completes it on the display or escalates.
        // Off by default: the extra hop taxes every task with latency and,
        // under subscription-based external agents, an API-key model the
        // deployment otherwise doesn't need.
        let cu_first_enabled = project.config.experimental.cu_first_routing;
        let task_for_agent: Option<String>;

        slog(&session_log, |l| {
            l.debug(&format!(
                "CU-first routing: enabled={}, force_direct={}, task={}",
                cu_first_enabled,
                envelope.force_direct,
                types::truncate_str(&envelope.task, 60)
            ))
        });

        if cu_first_enabled && !envelope.force_direct {
            // Auto-attach latest display frame(s) if none were explicitly provided
            let mut reference_images =
                resolve_frame_ids(&envelope.reference_frame_ids, &frame_registry).await;
            if reference_images.is_empty() {
                reference_images = auto_attach_display_frames(&frame_registry).await;
            }

            // Combine context-hint frames with user attachments so the CU
            // model also sees what the user pointed at when issuing the task.
            let mut cu_context_images = frame_images.clone();
            cu_context_images.extend(attachment_images.iter().cloned());

            match try_cu_first(
                &project_root,
                &reference_images,
                &cu_context_images,
                &envelope.task,
                &session_log,
                &log_dir,
                &bus,
                &session_registry,
            )
            .await
            {
                Some(Ok(CuTaskResult::Completed(stats))) => {
                    cumulative_stats.turns += stats.turns;
                    bus.send(AppEvent::PresenceLog {
                        message: format!("CU task complete ({} turns)", stats.turns),
                        level: None,
                        turn: None,
                    });
                    continue; // done
                }
                Some(Ok(CuTaskResult::Escalate { task })) => {
                    slog(&session_log, |l| {
                        l.info(&format!(
                            "CU escalated to agent: {}",
                            types::truncate_str(&task, 80)
                        ))
                    });
                    bus.send(AppEvent::PresenceLog {
                        message: format!("Escalating to agent: {}", types::truncate_str(&task, 80)),
                        level: None,
                        turn: None,
                    });
                    task_for_agent = Some(task);
                }
                Some(Err(e)) => {
                    slog(&session_log, |l| {
                        l.cu_task_error(&e.to_string(), Some("main agent"))
                    });
                    task_for_agent = Some(envelope.task.clone());
                }
                None => {
                    // No CU available (no display, no provider) — go to agent directly
                    task_for_agent = Some(envelope.task.clone());
                }
            }
        } else {
            task_for_agent = Some(envelope.task.clone());
        }

        // ── Regular agent path (for escalated or non-CU tasks) ──
        let task_text = task_for_agent.unwrap_or_else(|| envelope.task.clone());

        // Re-read the agent backend each task: the web UI may have changed it.
        let agent_backend = shared_external_agent.read().await.clone();
        // Snapshot the current Codex runtime config. The backend latches its
        // per-session config at spawn/thread-start — a toggle in the UI takes
        // effect on the NEXT task by forcing an agent rebuild.
        let current_codex_config = shared_codex_config.read().await.clone();
        let current_claude_config = shared_claude_config.read().await.clone();

        // Teardown conditions:
        //  - backend changed (any agent)
        //  - backend is Codex and any of the Codex-locked fields differ
        let codex_config_changed =
            matches!(agent_backend, Some(external_agent::AgentBackend::Codex))
                && persistent_codex_config
                    .as_ref()
                    .is_some_and(|prev| !codex_runtime_config_equal(prev, &current_codex_config));
        let claude_config_changed = matches!(
            agent_backend,
            Some(external_agent::AgentBackend::ClaudeCode)
        ) && persistent_claude_config
            .as_ref()
            .is_some_and(|prev| !claude_runtime_config_equal(prev, &current_claude_config));

        if persistent_agent.is_some()
            && (agent_backend != persistent_agent_backend
                || codex_config_changed
                || claude_config_changed)
        {
            if codex_config_changed {
                slog(&session_log, |l| {
                    l.info("Codex config changed; rebuilding agent for next task")
                });
            }
            if claude_config_changed {
                slog(&session_log, |l| {
                    l.info("Claude Code config changed; rebuilding agent for next task")
                });
            }
            persistent_agent = None;
            persistent_thread = None;
            persistent_event_rx = None;
            persistent_codex_config = None;
            persistent_claude_config = None;
            persistent_diff_tracker = ExternalDiffDeltaTracker::default();
            persistent_pending_runtime_steers.clear();
            persistent_handled_steer_ids.clear();
            persistent_open_side_threads.clear();
            persistent_side_rounds.clear();
            persistent_side_turn_revisions.clear();
        }

        if let Some(ref backend) = agent_backend {
            // ── External agent path ──
            // The external agent manages its own conversation; we keep the
            // agent + thread alive across tasks dispatched by presence.
            if persistent_agent.is_none() {
                persistent_pending_managed_context_replays.clear();
                persistent_managed_context_recovery_kickstarts_without_rewind = 0;
                persistent_managed_context_surgical_recoveries = 0;
                let mut proj = match Project::from_root(project_root.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        bus.send(AppEvent::PresenceLog {
                            message: format!("Project error: {}", e),
                            level: Some(types::LogLevel::Error),
                            turn: None,
                        });
                        continue;
                    }
                };
                // Apply the live runtime config on top of what was loaded
                // from TOML. The control plane writes TOML synchronously on
                // each change, so normally the two agree — but there's no
                // ordering guarantee between the save and the next
                // `from_root`, and `shared_codex_config` is always the
                // authoritative "what the user just chose" source.
                if matches!(backend, external_agent::AgentBackend::Codex) {
                    let cx = &mut proj.config.agent.codex;
                    cx.command = current_codex_config.command.clone();
                    cx.sandbox = current_codex_config.sandbox.clone();
                    cx.approval_policy = current_codex_config.approval_policy.clone();
                    cx.model = current_codex_config.model.clone();
                    cx.reasoning_effort = current_codex_config.reasoning_effort.clone();
                    cx.service_tier = current_codex_config.service_tier.clone();
                    cx.web_search = current_codex_config.web_search;
                    cx.network_access = current_codex_config.network_access;
                    cx.writable_roots = current_codex_config.writable_roots.clone();
                    cx.managed_context = current_codex_config.managed_context.clone();
                    cx.context_archive = current_codex_config.context_archive.clone();
                }
                if matches!(backend, external_agent::AgentBackend::ClaudeCode) {
                    let cc = &mut proj.config.agent.claude_code;
                    cc.model = current_claude_config.model.clone();
                    cc.permission_mode = current_claude_config.permission_mode.clone();
                    cc.allowed_tools = current_claude_config.allowed_tools.clone();
                }
                // The first agent build may be resuming a session from a
                // startup `--resume`/`--continue`. That session's persisted
                // per-session config (managed context, sandbox, …) overrides
                // the shared runtime config applied above — but only for the
                // build that consumes the startup resume token. Later rebuilds
                // start fresh threads and use the live shared config.
                let startup_resume = startup_resume_session.take();
                let startup_overrides = if startup_resume.is_some() {
                    startup_resume_session_config.take().filter(|config| {
                        config
                            .source
                            .as_deref()
                            .is_none_or(|source| source == backend.as_short_str())
                    })
                } else {
                    None
                };
                if let Some(config) = startup_overrides.as_ref() {
                    session_config::apply_to_project(&mut proj, backend, config);
                }
                let (agent, thread, event_rx) = match create_external_agent(
                    backend,
                    &proj,
                    &session_log,
                    web_port,
                    startup_resume,
                    session_log_id(&session_log),
                    startup_overrides
                        .as_ref()
                        .and_then(|config| config.codex_service_tier.clone()),
                    startup_overrides
                        .as_ref()
                        .and_then(|config| config.codex_home.clone()),
                )
                .await
                {
                    Ok(result) => result,
                    Err(e) => {
                        bus.send(AppEvent::PresenceLog {
                            message: format!("External agent error: {}", e),
                            level: Some(types::LogLevel::Error),
                            turn: None,
                        });
                        continue;
                    }
                };
                slog(&session_log, |l| {
                    l.debug(&format!(
                        "Mode: external agent ({}) via presence, thread: {}",
                        backend, thread.thread_id
                    ))
                });
                // A non-canonical thread id (Claude Code's placeholder until
                // the stream announces the real session id) must not be
                // recorded as a backend alias: frontends would retarget
                // status/phase updates at a window that never exists. The
                // real id arrives via AgentEvent::NativeSessionId.
                if backend.thread_id_is_canonical(&thread.thread_id) {
                    emit_external_session_identity(
                        &bus,
                        session_log_id(&session_log),
                        backend.as_short_str(),
                        &thread.thread_id,
                    );
                }
                if *backend == external_agent::AgentBackend::ClaudeCode {
                    emit_claude_code_session_capabilities(
                        &bus,
                        session_log_id(&session_log).as_deref(),
                    );
                }
                persistent_agent = Some(agent);
                persistent_thread = Some(thread);
                persistent_event_rx = Some(event_rx);
                persistent_diff_tracker = ExternalDiffDeltaTracker::default();
                persistent_pending_runtime_steers.clear();
                persistent_handled_steer_ids.clear();
                persistent_open_side_threads.clear();
                persistent_side_rounds.clear();
                persistent_side_turn_revisions.clear();
                persistent_agent_backend = agent_backend.clone();
                // Remember the Codex config this agent was spawned with so
                // we can detect drift at the next task and rebuild.
                persistent_codex_config =
                    if matches!(agent_backend, Some(external_agent::AgentBackend::Codex)) {
                        Some(current_codex_config.clone())
                    } else {
                        None
                    };
                persistent_claude_config = if matches!(
                    agent_backend,
                    Some(external_agent::AgentBackend::ClaudeCode)
                ) {
                    Some(current_claude_config.clone())
                } else {
                    None
                };
            }

            let session_dir = session_log
                .lock()
                .ok()
                .map(|l| l.dir().to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            let initial_attachments = if envelope.attachment_frame_ids.is_empty() {
                UserAttachments::default()
            } else {
                UserAttachments::from_items(
                    resolve_attachments(
                        &envelope.attachment_frame_ids,
                        &frame_registry,
                        &session_dir,
                        &project.root,
                    )
                    .await,
                )
            };
            let mut initial_followup =
                FollowUpMessage::with_attachments(task_text.clone(), initial_attachments);
            initial_followup.steer_id = envelope.steer_id.clone();
            let mut next_persistent_turn = Some(initial_followup);

            while let Some(active_followup) = next_persistent_turn.take() {
                let agent = persistent_agent.as_mut().unwrap();
                // An owned snapshot rather than a borrow: the post-drain
                // native-id upgrade below needs `persistent_thread` mutable.
                let thread_id_at_turn_start = persistent_thread
                    .as_ref()
                    .map(|thread| thread.thread_id.clone())
                    .unwrap();
                let thread_value = external_agent::AgentThread {
                    thread_id: thread_id_at_turn_start.clone(),
                };
                let thread = &thread_value;
                let drain_config = DrainConfig {
                    bus: &bus,
                    web_port,
                    session_id: session_log_id(&session_log),
                    alias_session_id: if matches!(backend, external_agent::AgentBackend::Codex) {
                        Some(thread_id_at_turn_start.clone())
                    } else {
                        None
                    },
                    backend_thread_id: Some(thread_id_at_turn_start.clone()),
                    autonomy: autonomy.clone(),
                    session_log: &session_log,
                    project_root: &project.root,
                    log_dir: &log_dir,
                    approval_registry: &approval_registry,
                    json_approval: None,
                    agent_source: Some(backend.to_string()),
                    suppress_agent_started: true,
                    persist_model_responses_inline: false,
                    headless: false,
                    context_injection: &context_injection,
                };
                let codex_managed_context_enabled =
                    matches!(backend, external_agent::AgentBackend::Codex)
                        && agent.supports_item_anchor_rewind();

                if codex_managed_context_enabled {
                    match refresh_external_context_usage_snapshot_for_preflight(
                        agent,
                        &drain_config,
                    )
                    .await
                    {
                        Ok(Some(snapshot)) => {
                            if let Some(decision) = managed_context_preflight_decision(
                                codex_managed_context_enabled,
                                &active_followup,
                                &snapshot,
                            ) {
                                match decision {
                                    ManagedContextPreflightDecision::Recovery {
                                        recovery_followup,
                                        held_followup,
                                        pressure,
                                    } => {
                                        let held_user_input = held_followup.is_some();
                                        if let Some(held) = held_followup {
                                            persistent_pending_managed_context_replays
                                                .push_back(held);
                                        }
                                        slog(&session_log, |l| {
                                            l.info(&format!(
                                                "Holding persistent Codex follow-up during managed-context {} pressure ({}/{} tokens); sending recovery kickstart",
                                                pressure.status,
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit
                                            ))
                                        });
                                        bus.send(AppEvent::LogEntry {
                                            session_id: session_log_id(&session_log),
                                            level: "info".to_string(),
                                            source: "Intendant".to_string(),
                                            content: format!(
                                                "Managed context is in rewind-only pressure ({}/{} tokens); {}.",
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit,
                                                if held_user_input {
                                                    "holding the user follow-up until recovery succeeds"
                                                } else {
                                                    "using the request as a recovery kickstart"
                                                }
                                            ),
                                            turn: None,
                                        });
                                        next_persistent_turn = Some(recovery_followup);
                                        continue;
                                    }
                                    ManagedContextPreflightDecision::DensityHandoff {
                                        handoff_followup,
                                        held_followup,
                                        pressure,
                                    } => {
                                        persistent_pending_managed_context_replays
                                            .push_back(held_followup);
                                        slog(&session_log, |l| {
                                            l.info(&format!(
                                                "Holding persistent Codex follow-up during managed-context density watch ({}/{} tokens, threshold {}); sending density handoff",
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit,
                                                pressure.recommended_rewind_limit
                                            ))
                                        });
                                        bus.send(AppEvent::LogEntry {
                                            session_id: session_log_id(&session_log),
                                            level: "info".to_string(),
                                            source: "Intendant".to_string(),
                                            content: format!(
                                                "Managed context is above the recommended density threshold ({}/{} tokens, threshold {}). Sending a density handoff before broad follow-up work. Normal tools remain allowed below rewind-only pressure.",
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit,
                                                pressure.recommended_rewind_limit
                                            ),
                                            turn: None,
                                        });
                                        next_persistent_turn = Some(handoff_followup);
                                        continue;
                                    }
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            slog(&session_log, |l| {
                                l.debug(&format!(
                                    "Could not read Codex context snapshot before persistent follow-up gate: {}",
                                    e
                                ))
                            });
                        }
                    }
                }

                // Send the task as a new turn in the existing thread, with any
                // user-attached frames passed as image inputs (Codex `LocalImage`,
                // Gemini ACP `Image` content block). Queued fallback steers are
                // prepended as `[User]` lines in the same user turn.
                let merged_text = drain_steer_queue_as_followup(
                    &context_injection,
                    &active_followup.text,
                    &bus,
                    session_log_id(&session_log).as_deref(),
                    drain_config.alias_session_id.as_deref(),
                )
                .unwrap_or_else(|| active_followup.text.clone());
                persistent_diff_tracker.seed_from_session_log(&project.root, &log_dir);
                let round = cumulative_stats.rounds.saturating_add(1);
                let status_text = if active_followup.text.trim().is_empty() {
                    &merged_text
                } else {
                    &active_followup.text
                };
                emit_external_turn_status(
                    &bus,
                    &autonomy,
                    session_log_id(&session_log).as_deref(),
                    round,
                    "thinking",
                    external_turn_status_task(agent.name(), round, status_text),
                )
                .await;
                let send_result = if active_followup.attachments.is_empty() {
                    agent.send_message(thread, &merged_text).await
                } else {
                    agent
                        .send_message_with_attachments(
                            thread,
                            &merged_text,
                            &active_followup.attachments.items,
                        )
                        .await
                };
                if let Err(e) = send_result {
                    bus.send(AppEvent::PresenceLog {
                        message: format!("External agent send error: {}", e),
                        level: Some(types::LogLevel::Error),
                        turn: None,
                    });
                    break;
                }
                if let Some(id) = active_followup.steer_id.as_deref() {
                    bus.send(AppEvent::SteerDelivered {
                        session_id: session_log_id(&session_log),
                        id: id.to_string(),
                        mid_turn: false,
                    });
                }

                let event_rx = persistent_event_rx.as_mut().unwrap();
                let mut side_session_state = ExternalSideSessionState {
                    open_side_threads: &mut persistent_open_side_threads,
                    side_rounds: &mut persistent_side_rounds,
                    side_turn_revisions: &mut persistent_side_turn_revisions,
                };
                let outcome = drain_external_agent_events(
                    agent,
                    event_rx,
                    &mut turn_bus_rx,
                    &drain_config,
                    &mut cumulative_stats,
                    &mut persistent_diff_tracker,
                    &mut persistent_pending_runtime_steers,
                    &mut persistent_handled_steer_ids,
                    &mut persistent_cancelled_follow_ups,
                    &mut codex_thread_action_dedupe,
                    Some(&mut side_session_state),
                    active_followup.managed_context_recovery_kickstart,
                    active_followup.managed_context_density_handoff,
                    active_followup.managed_context_density_handoff_completed,
                )
                .await;

                // A native id announced mid-turn (Claude Code's first turn)
                // upgrades the persistent thread handle, so this loop's
                // dynamic matchers (thread actions, follow-up cancels — they
                // read `persistent_thread` live) accept controls addressed
                // to the upgraded id.
                if let Some(native) = cumulative_stats.announced_native_session_id.take() {
                    let is_canonical = drain_config
                        .agent_source
                        .as_deref()
                        .and_then(external_agent::AgentBackend::from_str_loose)
                        .is_some_and(|backend| backend.thread_id_is_canonical(&native));
                    if is_canonical {
                        if let Some(thread) = persistent_thread.as_mut() {
                            if thread.thread_id != native {
                                slog(drain_config.session_log, |l| {
                                    l.info(&format!(
                                        "External session address upgraded to native id {}",
                                        short_external_session_id(&native)
                                    ))
                                });
                                thread.thread_id = native;
                            }
                        }
                    }
                }

                match outcome {
                    DrainOutcome::TurnCompleted {
                        message,
                        turns_in_round,
                    } => {
                        cumulative_stats.turns += 1;
                        cumulative_stats.rounds += 1;
                        if codex_managed_context_enabled {
                            match refresh_external_context_usage_snapshot(agent, &drain_config)
                                .await
                            {
                                Ok(Some(snapshot)) => {
                                    if let Some(pressure) =
                                        managed_context_rewind_only_pressure(&snapshot)
                                    {
                                        persistent_managed_context_recovery_kickstarts_without_rewind =
                                            persistent_managed_context_recovery_kickstarts_without_rewind
                                                .saturating_add(1);
                                        if persistent_managed_context_recovery_kickstarts_without_rewind
                                            < MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND
                                        {
                                            let held_user_input =
                                                !persistent_pending_managed_context_replays
                                                    .is_empty();
                                            let recovery_text =
                                                managed_context_recovery_kickstart_text(
                                                    pressure,
                                                    held_user_input,
                                                );
                                            let turn_kind = if active_followup
                                                .managed_context_recovery_kickstart
                                            {
                                                "recovery kickstart"
                                            } else {
                                                "managed Codex turn"
                                            };
                                            slog(&session_log, |l| {
                                                l.warn(&format!(
                                                    "Persistent managed-context {turn_kind} completed without a context rewind while pressure remains {}/{} tokens; retrying recovery",
                                                    pressure.used_tokens,
                                                    pressure.rewind_only_limit
                                                ))
                                            });
                                            bus.send(AppEvent::RoundComplete {
                                                session_id: session_log_id(&session_log),
                                                round: cumulative_stats.rounds,
                                                turns_in_round,
                                                native_message_count: None,
                                            });
                                            next_persistent_turn = Some(
                                                FollowUpMessage::text(recovery_text)
                                                    .managed_context_recovery_kickstart(),
                                            );
                                            continue;
                                        }
                                        // Backstop: model-driven recovery
                                        // exhausted its kickstart budget
                                        // (step-limit exhaustion each time);
                                        // surgical rewind instead of ending
                                        // the managed conversation.
                                        let mut surgical_failure = None;
                                        if managed_context_surgical_recovery_available(
                                            persistent_managed_context_surgical_recoveries,
                                        ) {
                                            match attempt_supervisor_surgical_context_rewind(
                                                agent,
                                                &thread.thread_id,
                                                &drain_config,
                                                (!task_text.trim().is_empty())
                                                    .then_some(task_text.as_str()),
                                                &mut persistent_pending_managed_context_replays,
                                            )
                                            .await
                                            {
                                                Ok(continuation) => {
                                                    persistent_managed_context_surgical_recoveries =
                                                        persistent_managed_context_surgical_recoveries
                                                            .saturating_add(1);
                                                    persistent_managed_context_recovery_kickstarts_without_rewind = 0;
                                                    let content = format!(
                                                        "Persistent managed-context recovery exhausted {} kickstarts without a rewind at {}/{} tokens; Intendant performed a surgical rewind ({} of {}) and is resuming the session.",
                                                        MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND,
                                                        pressure.used_tokens,
                                                        pressure.rewind_only_limit,
                                                        persistent_managed_context_surgical_recoveries,
                                                        MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES,
                                                    );
                                                    slog(&session_log, |l| l.warn(&content));
                                                    bus.send(AppEvent::LogEntry {
                                                        session_id: session_log_id(&session_log),
                                                        level: "warn".to_string(),
                                                        source: "Intendant".to_string(),
                                                        content,
                                                        turn: None,
                                                    });
                                                    bus.send(AppEvent::RoundComplete {
                                                        session_id: session_log_id(&session_log),
                                                        round: cumulative_stats.rounds,
                                                        turns_in_round,
                                                        native_message_count: None,
                                                    });
                                                    next_persistent_turn = Some(continuation);
                                                    continue;
                                                }
                                                Err(e) => surgical_failure = Some(e),
                                            }
                                        }
                                        let mut message = format!(
                                            "Managed-context recovery completed without rewind_context while context remains above the rewind-only threshold ({}/{} tokens); refusing to send normal follow-ups.",
                                            pressure.used_tokens,
                                            pressure.rewind_only_limit
                                        );
                                        match surgical_failure {
                                            Some(failure) => message.push_str(&format!(
                                                " Supervisor surgical rewind also failed: {failure}"
                                            )),
                                            None => message.push_str(&format!(
                                                " Supervisor surgical recovery budget ({} per session) is exhausted.",
                                                MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES
                                            )),
                                        }
                                        slog(&session_log, |l| l.warn(&message));
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        bus.send(AppEvent::LoopError(message));
                                        break;
                                    }

                                    persistent_managed_context_recovery_kickstarts_without_rewind =
                                        0;
                                    if managed_context_recovery_without_rewind_blocks_held_replay(
                                        active_followup.managed_context_recovery_kickstart,
                                        &persistent_pending_managed_context_replays,
                                    ) {
                                        let message = "Managed-context recovery turn completed without rewind_context; refusing to replay held normal follow-up until a successful rewind lowers context pressure.".to_string();
                                        slog(&session_log, |l| l.warn(&message));
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        bus.send(AppEvent::LoopError(message));
                                        break;
                                    }
                                    if let Some(mut replay) =
                                        persistent_pending_managed_context_replays.pop_front()
                                    {
                                        if active_followup.managed_context_density_handoff {
                                            replay = replay.after_managed_context_density_handoff();
                                            slog(&session_log, |l| {
                                                l.info(
                                                    "Persistent managed-context density handoff completed without a context rewind; replaying held follow-up",
                                                )
                                            });
                                        } else {
                                            slog(&session_log, |l| {
                                                l.warn(
                                                    "Persistent managed-context pressure cleared without a context rewind; replaying held follow-up",
                                                )
                                            });
                                        }
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        next_persistent_turn = Some(replay);
                                        continue;
                                    }
                                    if managed_context_post_turn_density_handoff_enabled(
                                        active_followup.managed_context_recovery_kickstart,
                                        active_followup.managed_context_density_handoff,
                                        active_followup.managed_context_density_handoff_completed,
                                    ) {
                                        if let Some(pressure) =
                                            managed_context_density_pressure(&snapshot)
                                        {
                                            let handoff_text =
                                                managed_context_density_handoff_text(pressure);
                                            slog(&session_log, |l| {
                                                l.info(&format!(
                                                    "Persistent managed Codex completed at density-watch pressure ({}/{} tokens); sending one-shot context handoff before waiting for follow-up",
                                                    pressure.used_tokens,
                                                    pressure.rewind_only_limit
                                                ))
                                            });
                                            bus.send(AppEvent::RoundComplete {
                                                session_id: session_log_id(&session_log),
                                                round: cumulative_stats.rounds,
                                                turns_in_round,
                                                native_message_count: None,
                                            });
                                            next_persistent_turn = Some(
                                                FollowUpMessage::text(handoff_text)
                                                    .managed_context_density_handoff(),
                                            );
                                            continue;
                                        }
                                    }
                                }
                                Ok(None) => {
                                    if active_followup.managed_context_recovery_kickstart
                                        || !persistent_pending_managed_context_replays.is_empty()
                                    {
                                        let message = "Managed-context recovery completed without rewind_context, and Codex context pressure could not be re-read; refusing to send normal follow-ups.".to_string();
                                        slog(&session_log, |l| l.warn(&message));
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        bus.send(AppEvent::LoopError(message));
                                        break;
                                    }
                                }
                                Err(e) => {
                                    if active_followup.managed_context_recovery_kickstart
                                        || !persistent_pending_managed_context_replays.is_empty()
                                    {
                                        let message = format!(
                                            "Managed-context recovery completed without rewind_context, and Codex context pressure could not be re-read: {}; refusing to send normal follow-ups.",
                                            e
                                        );
                                        slog(&session_log, |l| l.warn(&message));
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        bus.send(AppEvent::LoopError(message));
                                        break;
                                    }
                                    slog(&session_log, |l| {
                                        l.debug(&format!(
                                            "Could not re-read Codex context pressure after persistent managed turn: {}",
                                            e
                                        ))
                                    });
                                }
                            }
                        }

                        bus.send(AppEvent::DoneSignal {
                            session_id: session_log_id(&session_log),
                            message: message.clone(),
                        });
                        bus.send(AppEvent::RoundComplete {
                            session_id: session_log_id(&session_log),
                            round: cumulative_stats.rounds,
                            turns_in_round,
                            native_message_count: None,
                        });
                    }
                    DrainOutcome::ContextRewindRequested {
                        request,
                        message,
                        turns_in_round,
                        turn_stop_status,
                    } => {
                        persistent_managed_context_recovery_kickstarts_without_rewind = 0;
                        cumulative_stats.turns += 1;
                        cumulative_stats.rounds += 1;
                        bus.send(AppEvent::RoundComplete {
                            session_id: session_log_id(&session_log),
                            round: cumulative_stats.rounds,
                            turns_in_round,
                            native_message_count: None,
                        });
                        match apply_external_context_rewind(
                            agent,
                            &thread.thread_id,
                            &request,
                            &drain_config,
                        )
                        .await
                        {
                            Ok(automatic_resume) => {
                                if let Some(mut continuation) = managed_context_rewind_continuation(
                                    &mut persistent_pending_managed_context_replays,
                                    &active_followup,
                                    automatic_resume,
                                    &turn_stop_status,
                                ) {
                                    if active_followup.managed_context_density_handoff {
                                        continuation =
                                            continuation.after_managed_context_density_handoff();
                                    }
                                    slog(&session_log, |l| {
                                        l.info(
                                            "Persistent managed-context rewind succeeded; continuing queued follow-up",
                                        )
                                    });
                                    next_persistent_turn = Some(continuation);
                                    continue;
                                }
                                bus.send(AppEvent::DoneSignal {
                                    session_id: session_log_id(&session_log),
                                    message: message.clone(),
                                });
                            }
                            Err(message) => {
                                emit_context_rewind_failure(&request, message, &drain_config);
                                bus.send(AppEvent::DoneSignal {
                                    session_id: session_log_id(&session_log),
                                    message: None,
                                });
                            }
                        }
                    }
                    DrainOutcome::RecoveryRequired {
                        message,
                        recovery_hint,
                        turns_in_round,
                    } => {
                        cumulative_stats.rounds += 1;
                        if codex_managed_context_enabled {
                            persistent_managed_context_recovery_kickstarts_without_rewind =
                                persistent_managed_context_recovery_kickstarts_without_rewind
                                    .saturating_add(1);
                            if persistent_managed_context_recovery_kickstarts_without_rewind
                                < MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND
                            {
                                let pressure = match refresh_external_context_usage_snapshot(
                                    agent,
                                    &drain_config,
                                )
                                .await
                                {
                                    Ok(Some(snapshot)) => {
                                        managed_context_recovery_pressure(&snapshot)
                                    }
                                    Ok(None) => None,
                                    Err(e) => {
                                        slog(&session_log, |l| {
                                            l.debug(&format!(
                                                "Could not read Codex context snapshot after persistent recovery-required outcome: {}",
                                                e
                                            ))
                                        });
                                        None
                                    }
                                };
                                let held_user_input =
                                    !persistent_pending_managed_context_replays.is_empty();
                                let recovery_text = pressure
                                    .map(|pressure| {
                                        managed_context_recovery_kickstart_text(
                                            pressure,
                                            held_user_input,
                                        )
                                    })
                                    .unwrap_or_else(|| {
                                        managed_context_backend_recovery_kickstart_text(
                                            &message,
                                            recovery_hint.as_deref(),
                                        )
                                    });
                                slog(&session_log, |l| {
                                    l.warn(
                                        "Persistent managed Codex reported recovery required; sending managed-context recovery kickstart instead of ending the session",
                                    )
                                });
                                bus.send(AppEvent::RoundComplete {
                                    session_id: session_log_id(&session_log),
                                    round: cumulative_stats.rounds,
                                    turns_in_round,
                                    native_message_count: None,
                                });
                                next_persistent_turn = Some(
                                    FollowUpMessage::text(recovery_text)
                                        .managed_context_recovery_kickstart(),
                                );
                                continue;
                            }
                            // Backstop: kickstart budget exhausted while the
                            // backend still reports recovery required (the
                            // recovery turns hit their step limit without a
                            // rewind). Surgical rewind instead of leaving the
                            // thread stuck above the rewind-only threshold.
                            if managed_context_surgical_recovery_available(
                                persistent_managed_context_surgical_recoveries,
                            ) {
                                match attempt_supervisor_surgical_context_rewind(
                                    agent,
                                    &thread.thread_id,
                                    &drain_config,
                                    (!task_text.trim().is_empty()).then_some(task_text.as_str()),
                                    &mut persistent_pending_managed_context_replays,
                                )
                                .await
                                {
                                    Ok(continuation) => {
                                        persistent_managed_context_surgical_recoveries =
                                            persistent_managed_context_surgical_recoveries
                                                .saturating_add(1);
                                        persistent_managed_context_recovery_kickstarts_without_rewind = 0;
                                        let content = format!(
                                            "Persistent managed Codex kept reporting backend recovery required after {} kickstarts without a rewind; Intendant performed a surgical rewind ({} of {}) and is resuming the session.",
                                            MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND,
                                            persistent_managed_context_surgical_recoveries,
                                            MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES,
                                        );
                                        slog(&session_log, |l| l.warn(&content));
                                        bus.send(AppEvent::LogEntry {
                                            session_id: session_log_id(&session_log),
                                            level: "warn".to_string(),
                                            source: "Intendant".to_string(),
                                            content,
                                            turn: None,
                                        });
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: session_log_id(&session_log),
                                            round: cumulative_stats.rounds,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        next_persistent_turn = Some(continuation);
                                        continue;
                                    }
                                    Err(e) => {
                                        slog(&session_log, |l| {
                                            l.warn(&format!(
                                                "Supervisor surgical rewind failed after recovery-required exhaustion: {e}"
                                            ))
                                        });
                                    }
                                }
                            }
                        }
                        bus.send(AppEvent::RoundComplete {
                            session_id: session_log_id(&session_log),
                            round: cumulative_stats.rounds,
                            turns_in_round,
                            native_message_count: None,
                        });
                        bus.send(AppEvent::PresenceLog {
                            message: recovery_required_message(&message, recovery_hint.as_deref()),
                            level: Some(types::LogLevel::Warn),
                            turn: None,
                        });
                    }
                    DrainOutcome::Interrupted { reason } => {
                        bus.send(AppEvent::PresenceLog {
                            message: format!("External agent interrupted: {}", reason),
                            level: None,
                            turn: None,
                        });
                        cumulative_stats.rounds += 1;
                    }
                    DrainOutcome::Terminated { reason, .. } => {
                        bus.send(AppEvent::PresenceLog {
                            message: format!("External agent terminated: {}", reason),
                            level: Some(types::LogLevel::Error),
                            turn: None,
                        });
                        persistent_agent = None;
                        persistent_thread = None;
                        persistent_event_rx = None;
                        persistent_diff_tracker = ExternalDiffDeltaTracker::default();
                        persistent_pending_runtime_steers.clear();
                        persistent_handled_steer_ids.clear();
                        persistent_open_side_threads.clear();
                        persistent_side_rounds.clear();
                        persistent_side_turn_revisions.clear();
                        persistent_pending_managed_context_replays.clear();
                        break;
                    }
                    DrainOutcome::ChannelClosed => {
                        persistent_agent = None;
                        persistent_thread = None;
                        persistent_event_rx = None;
                        persistent_diff_tracker = ExternalDiffDeltaTracker::default();
                        persistent_pending_runtime_steers.clear();
                        persistent_handled_steer_ids.clear();
                        persistent_open_side_threads.clear();
                        persistent_side_rounds.clear();
                        persistent_side_turn_revisions.clear();
                        persistent_pending_managed_context_replays.clear();
                        break;
                    }
                }
            }
            turn_bus_rx = bus.subscribe();
        } else {
            // ── Native agent path ──
            if persistent_conv.is_none() {
                // ── First task: full initialization ──
                let proj = match Project::from_root(project_root.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        bus.send(AppEvent::PresenceLog {
                            message: format!("Project error: {}", e),
                            level: Some(types::LogLevel::Error),
                            turn: None,
                        });
                        continue;
                    }
                };

                // CU tasks are handled by the ephemeral path above; this is the
                // persistent conversation path for regular coding tasks.
                let mut task_provider = match provider::select_provider() {
                    Ok(p) => p,
                    Err(e) => {
                        bus.send(AppEvent::PresenceLog {
                            message: format!("Provider error: {}", e),
                            level: Some(types::LogLevel::Error),
                            turn: None,
                        });
                        continue;
                    }
                };
                task_provider.set_cu_enabled(true);

                slog(&session_log, |l| {
                    l.info(&format!(
                        "Mode: direct (provider: {}, context: {})",
                        task_provider.name(),
                        task_provider.context_window()
                    ));
                });

                let role = sub_agent::SubAgentRole::Custom("direct".to_string());
                let system_prompt = if task_provider.use_tools() {
                    prompts::resolve_system_prompt_for_tools(&role, Some(&proj.root))?
                } else {
                    prompts::resolve_system_prompt(&role, Some(&proj.root))?
                };

                let mut conv = Conversation::new(system_prompt, task_provider.context_window());
                setup_fresh_conversation_no_task(&mut conv, &proj);

                // Frame directory awareness
                let frames_dir = log_dir.join("frames");
                conv.add_user(format!(
                    "[System] Video frames from the user's camera are stored at: {}\n\
                     Each frame is a JPEG named by frame ID (e.g., cam0-f00001.jpg).\n\
                     When you receive frame references, you can read them from this path.",
                    frames_dir.display()
                ));
                conv.add_assistant("Understood.".to_string());

                // Add task with optional frame images. Combine context-hint
                // frames (from `frames:` hints) with user-attached frames
                // (from the dashboard's "Attach" buttons) — they're both
                // image content the model should see alongside the task.
                let mut combined_images = frame_images;
                combined_images.extend(attachment_images.iter().cloned());
                if combined_images.is_empty() {
                    conv.add_user(task_text.clone());
                } else {
                    conv.add_user_with_images(task_text.clone(), combined_images);
                }

                persistent_project = Some(proj);
                persistent_provider = Some(task_provider);
                persistent_conv = Some(conv);
            } else {
                // ── Subsequent task: inject into existing conversation ──
                let Some(conv) = persistent_conv.as_mut() else {
                    unreachable!("persistent conversation was initialized above");
                };

                let resolved = conv.resolve_dangling_tool_calls();
                if resolved > 0 {
                    slog(&session_log, |l| {
                        l.info(&format!(
                            "Resolved {} dangling tool call(s) from previous round",
                            resolved
                        ))
                    });
                }

                let mut combined_images = frame_images;
                combined_images.extend(attachment_images.iter().cloned());
                if combined_images.is_empty() {
                    conv.add_user(format!("[New Task] {}", task_text));
                } else {
                    conv.add_user_with_images(format!("[New Task] {}", task_text), combined_images);
                }
            }

            if let Some(id) = envelope.steer_id.as_deref() {
                bus.send(AppEvent::SteerDelivered {
                    session_id: session_log_id(&session_log),
                    id: id.to_string(),
                    mid_turn: false,
                });
            }

            // Run one round (agent loop until done/budget/error)
            let (follow_up_tx, mut follow_up_rx) = tokio::sync::mpsc::channel::<FollowUpMessage>(1);
            drop(follow_up_tx); // single-round per task dispatch

            let result = run_round_loop(
                persistent_provider.as_ref().unwrap().as_ref(),
                persistent_conv.as_mut().unwrap(),
                persistent_project.as_ref().unwrap(),
                None, // not sub-agent
                &bus,
                autonomy.clone(),
                session_log.clone(),
                &log_dir,
                None, // no MCP
                &mut follow_up_rx,
                None, // no JSON approval
                &approval_registry,
                &context_injection, // shared with presence
                Some(&session_registry),
                false, // not headless
                None,  // presence mode has no session supervisor
            )
            .await;

            match result {
                Ok(stats) => {
                    cumulative_stats.turns += stats.turns;
                    cumulative_stats.rounds += stats.rounds;
                    cumulative_stats.usage.prompt_tokens += stats.usage.prompt_tokens;
                    cumulative_stats.usage.completion_tokens += stats.usage.completion_tokens;
                    cumulative_stats.usage.total_tokens += stats.usage.total_tokens;
                }
                Err(e) => {
                    // Log error but DON'T discard conversation — it persists
                    bus.send(AppEvent::PresenceLog {
                        message: format!("Task error: {}", e),
                        level: Some(types::LogLevel::Error),
                        turn: None,
                    });
                }
            }
        }
    }

    Ok(cumulative_stats)
}

/// Configuration for a native in-process session beyond plain direct mode:
/// which prompt the loop runs under, whether it carries the supervised
/// orchestration handle, and whether it runs as a sub-agent child.
///
/// Orchestration used to be a separate subprocess mode (`run_user_mode`,
/// which spawned the orchestrator as a child process and polled its
/// progress/result files); it is now just a differently-configured
/// internal session, and sub-agents are supervised child sessions.
pub(crate) struct NativeSessionConfig {
    /// Resolves the system prompt (SysPrompt role files). Custom roles
    /// fall back to the base prompt.
    pub(crate) role: sub_agent::SubAgentRole,
    /// Replaces the role-resolved system prompt wholesale (the
    /// INTENDANT_SYSTEM_PROMPT semantic, session-scoped).
    pub(crate) system_prompt_override: Option<String>,
    /// Inject the project knowledge store into fresh conversations.
    pub(crate) inherit_memory: bool,
    /// Present on supervised (daemon) sessions: grants the loop the
    /// spawn_sub_agent / wait_sub_agents / submit_result capability.
    pub(crate) orchestration: Option<session_supervisor::SessionOrchestration>,
    /// Present when this session runs as a sub-agent child: (name, role).
    /// Children end when their task ends instead of idling for follow-ups.
    pub(crate) sub_agent_identity: Option<(String, sub_agent::SubAgentRole)>,
}

impl NativeSessionConfig {
    /// Plain direct session: base prompt, no supervision extras. The shape
    /// every non-daemon CLI path runs — orchestration (sub-agent spawning)
    /// requires the daemon's session supervisor.
    pub(crate) fn direct() -> Self {
        Self {
            role: sub_agent::SubAgentRole::Custom("direct".to_string()),
            system_prompt_override: None,
            inherit_memory: false,
            orchestration: None,
            sub_agent_identity: None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_direct_mode(
    mut provider: Box<dyn provider::ChatProvider>,
    task: String,

    project: Project,
    bus: EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: PathBuf,
    mcp_mgr: Option<mcp_client::McpClientManager>,
    mut follow_up_rx: FollowUpReceiver,
    json_approval: Option<JsonApprovalSlot>,
    approval_registry: event::ApprovalRegistry,
    context_injection: event::ContextInjectionQueue,
    session_registry: Option<display::SharedSessionRegistry>,
    headless: bool,
    attachments: UserAttachments,
    native: NativeSessionConfig,
) -> Result<LoopStats, CallerError> {
    let role = native.role.clone();
    // Prompt precedence: session-scoped override (spawn_sub_agent's
    // system_prompt) > the INTENDANT_SYSTEM_PROMPT env escape hatch for
    // direct CLI invocations > the role-resolved SysPrompt files. An
    // override replaces the resolved prompt wholesale.
    let system_prompt_override = native.system_prompt_override.clone().or_else(|| {
        env::var("INTENDANT_SYSTEM_PROMPT")
            .ok()
            .filter(|p| !p.trim().is_empty())
    });
    let system_prompt = match system_prompt_override {
        Some(prompt) => prompt,
        None if provider.use_tools() => {
            prompts::resolve_system_prompt_for_tools(&role, Some(&project.root))?
        }
        None => prompts::resolve_system_prompt(&role, Some(&project.root))?,
    };

    let mode_label = if native.sub_agent_identity.is_some() {
        "sub-agent"
    } else if matches!(role, sub_agent::SubAgentRole::Orchestrator) {
        "orchestrate"
    } else {
        "direct"
    };
    slog(&session_log, |l| {
        l.info(&format!(
            "Mode: {} (provider: {}, context: {})",
            mode_label,
            provider.name(),
            provider.context_window()
        ));
    });
    if headless {
        println!(
            "Provider: {} (context window: {})",
            provider.name(),
            provider.context_window()
        );
    }

    // Try to resume from saved conversation if it exists in this session dir
    let conv_path = log_dir.join("conversation.jsonl");
    let attachment_images = attachments.conversation_images();
    let mut fresh_conversation = false;
    let mut conversation = if conv_path.exists() {
        match Conversation::load_from_file(&conv_path, provider.context_window()) {
            Ok(mut conv) => {
                slog(&session_log, |l| {
                    l.info(&format!(
                        "Resumed conversation ({} messages, turn {})",
                        conv.len(),
                        conv.turn()
                    ))
                });
                // Append the new task as a continuation message
                let resume_msg = attachments
                    .text_with_file_prelude(&format!("[Session resumed] Continue with: {}", task));
                if attachment_images.is_empty() {
                    conv.add_user(resume_msg);
                } else {
                    conv.add_user_with_images(resume_msg, attachment_images.clone());
                }
                conv
            }
            Err(e) => {
                slog(&session_log, |l| {
                    l.warn(&format!(
                        "Failed to load conversation, starting fresh: {}",
                        e
                    ))
                });
                fresh_conversation = true;
                let mut conv = Conversation::new(system_prompt, provider.context_window());
                setup_fresh_conversation_with_attachments(
                    &mut conv,
                    &project,
                    &attachments.text_with_file_prelude(&task),
                    attachment_images.clone(),
                );
                conv
            }
        }
    } else {
        fresh_conversation = true;
        let mut conv = Conversation::new(system_prompt, provider.context_window());
        setup_fresh_conversation_with_attachments(
            &mut conv,
            &project,
            &attachments.text_with_file_prelude(&task),
            attachment_images.clone(),
        );
        conv
    };

    // Inject inherited project knowledge (sub-agents spawned with
    // inherit_memory). Resumed conversations already carry it.
    if native.inherit_memory && fresh_conversation && project.config.memory.enabled {
        if let Ok(kstore) = knowledge::load(&project.memory_path()) {
            let refs: Vec<&_> = kstore.entries.iter().collect();
            let msg = knowledge::format_for_injection(&refs);
            if !msg.is_empty() {
                conversation.add_user(msg);
                conversation.add_assistant(
                    "Acknowledged. I have loaded the project knowledge.".to_string(),
                );
            }
        }
    }

    // Register MCP tools so providers include them in API requests
    if let Some(ref mgr) = mcp_mgr {
        tools::register_extra_tools(mgr.all_tools());
    }

    // Enable native CU on the main provider. The "computer" tool type
    // requires no display dimensions — the model infers from screenshots.
    provider.set_cu_enabled(true);

    if headless {
        println!("Task: {}", task);
        println!("---");
    }

    run_round_loop(
        provider.as_ref(),
        &mut conversation,
        &project,
        native.sub_agent_identity.as_ref(),
        &bus,
        autonomy,
        session_log,
        &log_dir,
        mcp_mgr.as_ref(),
        &mut follow_up_rx,
        json_approval.as_ref(),
        &approval_registry,
        &context_injection,
        session_registry.as_ref(),
        headless,
        native.orchestration.as_ref(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_external_agent_mode(
    backend: external_agent::AgentBackend,
    task: String,
    project: Project,
    bus: EventBus,
    autonomy: SharedAutonomy,
    session_log: SharedSessionLog,
    log_dir: PathBuf,
    mut follow_up_rx: FollowUpReceiver,
    json_approval: Option<JsonApprovalSlot>,
    approval_registry: event::ApprovalRegistry,
    context_injection: event::ContextInjectionQueue,
    headless: bool,
    web_port: Option<u16>,
    attachments: UserAttachments,
    resume_session: Option<String>,
    codex_service_tier: Option<String>,
    codex_home: Option<String>,
    control_session_id: Option<String>,
    emit_session_started_after_identity: bool,
    ready_for_thread_actions: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<LoopStats, CallerError> {
    slog(&session_log, |l| {
        l.info(&format!("Mode: external agent ({})", backend));
    });
    if headless {
        println!("External agent: {}", backend);
        if task.trim().is_empty() {
            println!("Attached session; waiting for input");
        } else {
            println!("Task: {}", task);
        }
        println!("---");
    }

    // Construct, initialize, and start a thread for the external agent
    let resumed_external_session = resume_session.clone();
    let persist_model_responses_inline = control_session_id.is_some();
    let intendant_session_id = control_session_id.or_else(|| session_log_id(&session_log));
    let effective_codex_home = if backend == external_agent::AgentBackend::Codex {
        codex_home
            .as_deref()
            .and_then(|home| crate::session_config::normalize_codex_home(Some(home)))
            .or_else(crate::session_config::effective_codex_home)
    } else {
        None
    };
    let effective_codex_service_tier = if backend == external_agent::AgentBackend::Codex {
        codex_service_tier.clone().or_else(|| {
            project::normalize_codex_service_tier(
                project.config.agent.codex.service_tier.as_deref(),
            )
        })
    } else {
        None
    };
    if backend == external_agent::AgentBackend::Codex {
        emit_codex_session_capabilities_for_project(
            &bus,
            intendant_session_id.as_deref(),
            &project,
            effective_codex_service_tier.as_deref(),
        );
    } else if backend == external_agent::AgentBackend::ClaudeCode {
        emit_claude_code_session_capabilities(&bus, intendant_session_id.as_deref());
    }
    let (mut agent, thread, mut event_rx) = match create_external_agent(
        &backend,
        &project,
        &session_log,
        web_port,
        resume_session,
        intendant_session_id.clone(),
        effective_codex_service_tier,
        effective_codex_home.clone(),
    )
    .await
    {
        Ok(started) => started,
        Err(e) => {
            if emit_session_started_after_identity {
                if let Some(session_id) = intendant_session_id.clone() {
                    bus.send(AppEvent::SessionStarted {
                        session_id,
                        task: if task.trim().is_empty() {
                            None
                        } else {
                            Some(task.clone())
                        },
                    });
                }
            }
            return Err(e);
        }
    };
    let codex_managed_context_enabled =
        backend == external_agent::AgentBackend::Codex && agent.supports_item_anchor_rewind();
    let backend_session_id = thread.thread_id.clone();
    let mut session_agent_config = session_config::from_project(&backend, &project);
    if backend == external_agent::AgentBackend::Codex {
        session_agent_config.codex_service_tier = agent.service_tier().map(str::to_string);
        session_agent_config.codex_home = effective_codex_home;
    }
    // The spawner (session supervisor) may already have persisted
    // per-session facts to this log dir — fork lineage (`forked_from`),
    // per-session overrides — before launching this loop. Project defaults
    // must never clobber them.
    if let Some(existing) = session_config::read_log_dir_config(&log_dir) {
        session_agent_config.merge_missing_from(existing);
    }
    if let Err(e) = session_config::write_log_dir_config(&log_dir, &session_agent_config) {
        slog(&session_log, |l| {
            l.debug(&format!("Persist session launch config failed: {e}"))
        });
    }
    if backend.thread_id_is_canonical(&backend_session_id) {
        if let Err(e) = session_config::write_external_overlay(
            &platform::home_dir(),
            backend.as_short_str(),
            &backend_session_id,
            &session_agent_config,
        ) {
            slog(&session_log, |l| {
                l.debug(&format!("Persist external launch config failed: {e}"))
            });
        }
    }
    let mut live_session_id = if backend.thread_id_is_canonical(&backend_session_id) {
        Some(backend_session_id.clone())
    } else {
        intendant_session_id.clone()
    };
    // Placeholder thread ids (see thread_id_is_canonical) are withheld from
    // the identity stream: the real backend id is announced later via
    // AgentEvent::NativeSessionId and recording the placeholder would point
    // frontends' status routing at a never-materialized window.
    if backend.thread_id_is_canonical(&backend_session_id) {
        emit_external_session_identity(
            &bus,
            intendant_session_id
                .clone()
                .or_else(|| session_log_id(&session_log)),
            backend.as_short_str(),
            &backend_session_id,
        );
    }
    if backend == external_agent::AgentBackend::Codex {
        let service_tier = agent.service_tier().map(str::to_string);
        emit_codex_session_capabilities_for_project(
            &bus,
            intendant_session_id.as_deref(),
            &project,
            service_tier.as_deref(),
        );
        if live_session_id != intendant_session_id {
            emit_codex_session_capabilities_for_project(
                &bus,
                live_session_id.as_deref(),
                &project,
                service_tier.as_deref(),
            );
        }
    } else if backend == external_agent::AgentBackend::ClaudeCode {
        emit_claude_code_session_capabilities(&bus, intendant_session_id.as_deref());
        if live_session_id != intendant_session_id {
            emit_claude_code_session_capabilities(&bus, live_session_id.as_deref());
        }
    }
    if emit_session_started_after_identity {
        if let Some(session_id) = live_session_id.clone() {
            bus.send(AppEvent::SessionStarted {
                session_id,
                task: if task.trim().is_empty() {
                    None
                } else {
                    Some(task.clone())
                },
            });
        }
    }

    // Event loop
    let mut user_turn_revisions = match (
        &backend,
        resumed_external_session.as_deref(),
        backend_session_id.as_str(),
    ) {
        (external_agent::AgentBackend::Codex, Some(_), session_id) => {
            codex_user_turn_state_from_history(session_id).unwrap_or_default()
        }
        _ => UserTurnRevisionState::default(),
    };
    let mut round = user_turn_revisions.active_count() as usize;
    let mut stats = LoopStats::default();
    if backend == external_agent::AgentBackend::Codex {
        stats.codex_subagent_parent_threads = codex_subagent_parent_threads_from_log(&log_dir);
        for child_id in stats.codex_subagent_parent_threads.keys().cloned() {
            stats.codex_subagent_rounds.entry(child_id).or_insert(0);
        }
    }
    let mut diff_tracker = ExternalDiffDeltaTracker::default();
    let mut pending_runtime_steers: std::collections::VecDeque<PendingRuntimeSteer> =
        std::collections::VecDeque::new();
    let mut handled_steer_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cancelled_follow_ups: HashSet<String> = HashSet::new();
    let mut open_side_threads: HashMap<String, String> = HashMap::new();
    let mut side_rounds: HashMap<String, usize> = HashMap::new();
    let mut side_turn_revisions: HashMap<String, UserTurnRevisionState> = HashMap::new();
    let mut pending_managed_context_replays: std::collections::VecDeque<FollowUpMessage> =
        std::collections::VecDeque::new();
    let mut managed_context_recovery_kickstarts_without_rewind = 0u8;
    let mut managed_context_density_block_handoffs_without_relief = 0u8;
    let mut managed_context_surgical_recoveries = 0u8;
    // Task statement for surgical-recovery primers (the supervisor cannot
    // summarize the pruned span; it restates the task instead).
    let surgical_task_statement = (!task.trim().is_empty()).then(|| task.clone());
    let mut next_turn = if task.trim().is_empty() {
        None
    } else {
        Some(FollowUpMessage::with_attachments(task, attachments))
    };

    let mut drain_config = DrainConfig {
        bus: &bus,
        web_port,
        session_id: live_session_id.clone(),
        alias_session_id: if intendant_session_id != live_session_id {
            intendant_session_id.clone()
        } else {
            None
        },
        backend_thread_id: Some(backend_session_id.clone()),
        autonomy: autonomy.clone(),
        session_log: &session_log,
        project_root: &project.root,
        log_dir: &log_dir,
        approval_registry: &approval_registry,
        json_approval: json_approval.as_ref(),
        agent_source: Some(backend.to_string()),
        suppress_agent_started: false,
        persist_model_responses_inline,
        headless,
        context_injection: &context_injection,
    };
    // Use one control receiver across idle waits and active turn drains.
    // A second parked receiver would retain mid-turn controls and replay them
    // as new idle follow-ups after the turn completes.
    let mut external_control_rx = bus.subscribe();
    let mut codex_thread_action_dedupe = CodexThreadActionDedupe::default();
    if let Some(ready_tx) = ready_for_thread_actions {
        let _ = ready_tx.send(());
    }

    'outer: loop {
        let followup = match next_turn.take() {
            Some(turn) => turn,
            None => loop {
                if has_queued_steers_for_session(
                    &context_injection,
                    live_session_id.as_deref(),
                    drain_config.alias_session_id.as_deref(),
                ) {
                    break FollowUpMessage::text(String::new());
                }
                tokio::select! {
                    maybe_followup = follow_up_rx.recv() => {
                        match maybe_followup {
                            Some(followup) => {
                                if follow_up_message_was_cancelled(
                                    &mut cancelled_follow_ups,
                                    &followup,
                                ) {
                                    slog(&session_log, |l| {
                                        l.info("Skipped cancelled queued follow-up")
                                    });
                                    continue;
                                }
                                if let Some(id) = followup.steer_id.as_deref() {
                                    if steer_id_has_been_handled(&handled_steer_ids, id) {
                                        slog(&session_log, |l| {
                                            l.debug(&format!(
                                                "Ignoring duplicate queued steer {} already consumed by another delivery path",
                                                id
                                            ))
                                        });
                                        continue;
                                    }
                                    mark_steer_id_handled(&mut handled_steer_ids, id);
                                }
                                break followup;
                            }
                            None => {
                                slog(&session_log, |l| {
                                    l.info("Follow-up channel closed, exiting")
                                });
                                stats.terminal_outcome =
                                    Some("follow-up channel closed".to_string());
                                break 'outer;
                            }
                        }
                    }
                    maybe_event = event_rx.recv() => {
                        match maybe_event {
                            Some(event) => {
                                let (event_thread_id, event_turn_id, event) = event.into_scope();
                                if let Some(child_thread_id) =
                                    scoped_event_codex_subagent_thread_id(&event_thread_id, &stats)
                                {
                                    handle_idle_codex_subagent_event(
                                        &drain_config,
                                        &mut stats,
                                        child_thread_id,
                                        event,
                                    );
                                    continue;
                                }
                                match event {
                                    external_agent::AgentEvent::NativeSessionId { session_id } => {
                                        persist_native_backend_session_id(
                                            &drain_config,
                                            &session_id,
                                        );
                                        if backend.thread_id_is_canonical(&session_id) {
                                            rotate_external_identity(
                                                &session_id,
                                                &mut live_session_id,
                                                &mut drain_config,
                                            );
                                        }
                                    }
                                    external_agent::AgentEvent::GoalUpdated { goal } => {
                                        emit_external_session_goal(
                                            &drain_config,
                                            event_thread_id,
                                            Some(goal),
                                        );
                                    }
                                    external_agent::AgentEvent::GoalCleared => {
                                        emit_external_session_goal(
                                            &drain_config,
                                            event_thread_id,
                                            None,
                                        );
                                    }
                                    external_agent::AgentEvent::Terminated { reason, exit_code } => {
                                        let message = format!(
                                            "{} terminated while idle: {} (exit code: {:?})",
                                            agent.name(),
                                            reason,
                                            exit_code
                                        );
                                        slog(&session_log, |l| l.warn(&message));
                                        bus.send(AppEvent::LoopError(message));
                                        stats.terminal_outcome = Some(reason);
                                        break 'outer;
                                    }
                                    // Ambient diagnostics are not evidence of a
                                    // backend-initiated turn. Recording them inline and
                                    // staying idle matters: entering the observe drain on
                                    // one of these deadlocks the session — with no real
                                    // turn running the drain never sees a terminal event,
                                    // so queued follow-ups are never picked up again
                                    // (codex emits stderr `Log` lines right after a
                                    // resume attach, e.g. failing MCP-server logins).
                                    // Only turn-implying events (messages, reasoning,
                                    // tools, plan/diff updates, turn completion) may fall
                                    // through to the observe drain below.
                                    external_agent::AgentEvent::Log { level, message } => {
                                        slog(&session_log, |l| match level.as_str() {
                                            "warn" => l.warn(&message),
                                            "error" => l.error(&message),
                                            _ => l.info(&message),
                                        });
                                        bus.send(AppEvent::LogEntry {
                                            session_id: drain_config.session_id.clone(),
                                            level,
                                            source: drain_config
                                                .agent_source
                                                .clone()
                                                .unwrap_or_else(|| "worker".to_string()),
                                            content: message,
                                            turn: None,
                                        });
                                    }
                                    external_agent::AgentEvent::Usage { usage } => {
                                        let main = frontend::ModelUsageSnapshot {
                                            provider: usage.provider,
                                            model: usage.model,
                                            tokens_used: usage.tokens_used,
                                            context_window: usage.context_window,
                                            hard_context_window: usage.hard_context_window,
                                            usage_pct: usage.usage_pct,
                                            prompt_tokens: usage.prompt_tokens,
                                            completion_tokens: usage.completion_tokens,
                                            cached_tokens: usage.cached_tokens,
                                        };
                                        bus.send(AppEvent::UsageSnapshot {
                                            session_id: drain_config.session_id.clone(),
                                            main,
                                            presence: None,
                                        });
                                    }
                                    external_agent::AgentEvent::BackendError {
                                        message,
                                        code,
                                        details,
                                        will_retry,
                                        ..
                                    } => {
                                        let mut content = if let Some(code) = code.as_deref() {
                                            format!(
                                                "{} backend error while idle ({code}): {message}",
                                                agent.name()
                                            )
                                        } else {
                                            format!(
                                                "{} backend error while idle: {message}",
                                                agent.name()
                                            )
                                        };
                                        if let Some(details) =
                                            details.as_deref().filter(|s| !s.trim().is_empty())
                                        {
                                            content.push('\n');
                                            content.push_str(details.trim());
                                        }
                                        slog(&session_log, |l| {
                                            if will_retry {
                                                l.warn(&content)
                                            } else {
                                                l.error(&content)
                                            }
                                        });
                                        bus.send(AppEvent::LogEntry {
                                            session_id: drain_config.session_id.clone(),
                                            level: if will_retry { "warn" } else { "error" }
                                                .to_string(),
                                            source: external_agent_log_source(
                                                drain_config.agent_source.as_deref(),
                                            ),
                                            content,
                                            turn: None,
                                        });
                                    }
                                    other => {
                                        let event_targets_primary = scoped_event_targets_config(
                                            &event_thread_id,
                                            &live_session_id,
                                            &drain_config.alias_session_id,
                                        );
                                        let event_targets_side = event_thread_id
                                            .as_deref()
                                            .is_some_and(|id| open_side_threads.contains_key(id));
                                        if !event_targets_primary && !event_targets_side {
                                            continue;
                                        }

                                        let prefetched_event = external_agent::AgentEvent::scoped(
                                            event_thread_id.clone(),
                                            event_turn_id,
                                            other,
                                        );
                                        let observed_session_id =
                                            event_thread_id.clone().or_else(|| live_session_id.clone());
                                        let mut prefetched_events =
                                            std::collections::VecDeque::new();
                                        prefetched_events.push_back(prefetched_event);
                                        let mut side_session_state = ExternalSideSessionState {
                                            open_side_threads: &mut open_side_threads,
                                            side_rounds: &mut side_rounds,
                                            side_turn_revisions: &mut side_turn_revisions,
                                        };
                                        round += 1;
                                        stats.turns = 0;
                                        emit_external_turn_status(
                                            &bus,
                                            &autonomy,
                                            observed_session_id.as_deref(),
                                            round,
                                            "running",
                                            format!(
                                                "{} backend turn {} observed while idle",
                                                agent.name(),
                                                round
                                            ),
                                        )
                                        .await;
                                        let drain_outcome =
                                            drain_external_agent_events_with_prefetched(
                                                &mut agent,
                                                &mut event_rx,
                                                &mut external_control_rx,
                                                &drain_config,
                                                &mut stats,
                                                &mut diff_tracker,
                                                &mut pending_runtime_steers,
                                                &mut handled_steer_ids,
                                                &mut cancelled_follow_ups,
                                                &mut codex_thread_action_dedupe,
                                                &mut prefetched_events,
                                                Some(&mut side_session_state),
                                                false,
                                                false,
                                                false,
                                            )
                                            .await;
                                        if let Some(native) =
                                            stats.announced_native_session_id.take()
                                        {
                                            if backend.thread_id_is_canonical(&native) {
                                                slog(&session_log, |l| {
                                                    l.info(&format!(
                                                        "External session address upgraded to native id {}",
                                                        short_external_session_id(&native)
                                                    ))
                                                });
                                                rotate_external_identity(
                                                    &native,
                                                    &mut live_session_id,
                                                    &mut drain_config,
                                                );
                                            }
                                        }
                                        match drain_outcome {
                                            DrainOutcome::TurnCompleted {
                                                message,
                                                turns_in_round,
                                            } => {
                                                stats.rounds = round;
                                                record_external_done_and_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    live_session_id.as_deref(),
                                                    message.as_deref(),
                                                    round,
                                                    turns_in_round,
                                                );
                                                bus.send(AppEvent::DoneSignal {
                                                    session_id: live_session_id.clone(),
                                                    message: message.clone(),
                                                });
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                });
                                            }
                                            DrainOutcome::ContextRewindRequested {
                                                request,
                                                message,
                                                turns_in_round,
                                                ..
                                            } => {
                                                stats.rounds = round;
                                                record_external_done_and_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    live_session_id.as_deref(),
                                                    message.as_deref(),
                                                    round,
                                                    turns_in_round,
                                                );
                                                bus.send(AppEvent::DoneSignal {
                                                    session_id: live_session_id.clone(),
                                                    message: message.clone(),
                                                });
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                });
                                                emit_context_rewind_failure(
                                                    &request,
                                                    "context rewind was requested during a backend-started turn observed from idle; the turn was recorded, but the rewind was not applied automatically".to_string(),
                                                    &drain_config,
                                                );
                                            }
                                            DrainOutcome::RecoveryRequired {
                                                message,
                                                recovery_hint,
                                                turns_in_round,
                                            } => {
                                                stats.rounds = round;
                                                let message = recovery_required_message(
                                                    &message,
                                                    recovery_hint.as_deref(),
                                                );
                                                slog(&session_log, |l| l.warn(&message));
                                                record_external_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    round,
                                                    turns_in_round,
                                                );
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                });
                                                bus.send(AppEvent::LoopError(message));
                                                stats.terminal_outcome =
                                                    Some("recovery required".to_string());
                                                break 'outer;
                                            }
                                            DrainOutcome::Interrupted { reason } => {
                                                stats.rounds = round;
                                                slog(&session_log, |l| {
                                                    l.info(&format!(
                                                        "External agent interrupted while observed from idle: {}",
                                                        reason
                                                    ))
                                                });
                                                record_external_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    round,
                                                    stats.turns,
                                                );
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round: stats.turns,
                                                    native_message_count: None,
                                                });
                                            }
                                            DrainOutcome::Terminated { reason, exit_code } => {
                                                stats.rounds = round;
                                                slog(&session_log, |l| {
                                                    l.info(&format!(
                                                        "External agent terminated while observed from idle: {} (exit code: {:?})",
                                                        reason,
                                                        exit_code
                                                    ))
                                                });
                                                bus.send(AppEvent::TaskComplete {
                                                    session_id: live_session_id.clone(),
                                                    reason: reason.clone(),
                                                    summary: stats.last_response.clone(),
                                                });
                                                stats.terminal_outcome = Some(reason);
                                                break 'outer;
                                            }
                                            DrainOutcome::ChannelClosed => {
                                                slog(&session_log, |l| {
                                                    l.info(
                                                        "External agent event channel closed while observed from idle",
                                                    )
                                                });
                                                stats.terminal_outcome = Some(
                                                    "external agent event channel closed".to_string(),
                                                );
                                                break 'outer;
                                            }
                                        }
                                    }
                                }
                                continue;
                            }
                            None => {
                                slog(&session_log, |l| {
                                    l.info("External agent event channel closed, exiting")
                                });
                                stats.terminal_outcome =
                                    Some("external agent event channel closed".to_string());
                                break 'outer;
                            }
                        }
                    }
                    bus_event = external_control_rx.recv() => {
                        match bus_event {
                            Ok(AppEvent::SessionStopRequested { session_id, reason })
                                if event_targets_external_session_or_side(
                                    &session_id,
                                    &live_session_id,
                                    &drain_config.alias_session_id,
                                    &open_side_threads,
                                ) =>
                            {
                                slog(&session_log, |l| {
                                    l.info(&format!("Stop requested while idle: {}", reason))
                                });
                                stats.terminal_outcome = Some(reason);
                                break 'outer;
                            }
                            Ok(AppEvent::SteerCancelRequested {
                                session_id,
                                id,
                                reason,
                            }) => {
                                let Some((target_session_id, _target_kind)) =
                                    resolve_external_steer_target_session(
                                        &session_id,
                                        &live_session_id,
                                        &drain_config.alias_session_id,
                                        Some(&open_side_threads),
                                    )
                                else {
                                    continue;
                                };
                                let cancelled_queue = cancel_queued_steers_for_session(
                                    &context_injection,
                                    &bus,
                                    target_session_id.as_deref(),
                                    if target_session_id == live_session_id {
                                        drain_config.alias_session_id.as_deref()
                                    } else {
                                        None
                                    },
                                    id.as_deref(),
                                    &reason,
                                );
                                let cancelled_pending = cancel_pending_runtime_steers_for_session(
                                    &bus,
                                    &mut pending_runtime_steers,
                                    target_session_id.as_deref(),
                                    if target_session_id == live_session_id {
                                        drain_config.alias_session_id.as_deref()
                                    } else {
                                        None
                                    },
                                    id.as_deref(),
                                    &reason,
                                );
                                if cancelled_queue + cancelled_pending == 0 {
                                    if let Some(id) = id.filter(|id| !id.trim().is_empty()) {
                                        bus.send(AppEvent::SteerCancelled {
                                            session_id: target_session_id.or_else(|| live_session_id.clone()),
                                            id,
                                            reason,
                                        });
                                    }
                                }
                                continue;
                            }
                            Ok(AppEvent::FollowUpCancelRequested {
                                session_id,
                                id,
                                reason,
                            }) if event_targets_external_session_or_side(
                                &session_id,
                                &live_session_id,
                                &drain_config.alias_session_id,
                                &open_side_threads,
                            ) => {
                                let status_session =
                                    session_id.as_deref().or(live_session_id.as_deref());
                                record_cancelled_follow_up_id(
                                    &mut cancelled_follow_ups,
                                    &bus,
                                    status_session,
                                    id,
                                    &reason,
                                );
                                continue;
                            }
                            Ok(AppEvent::SteerRequested {
                                session_id,
                                text,
                                id,
                            }) if event_targets_external_session_or_side(
                                &session_id,
                                &live_session_id,
                                &drain_config.alias_session_id,
                                &open_side_threads,
                            ) => {
                                if steer_id_has_been_handled(&handled_steer_ids, &id) {
                                    slog(&session_log, |l| {
                                        l.debug(&format!(
                                            "Ignoring duplicate steer {} already consumed by another delivery path",
                                            id
                                        ))
                                    });
                                    continue;
                                }
                                mark_steer_id_handled(&mut handled_steer_ids, &id);
                                if maybe_handle_codex_fast_slash_steer(
                                    &mut agent,
                                    &text,
                                    session_id.clone(),
                                    id.clone(),
                                    &drain_config,
                                )
                                .await
                                {
                                    continue;
                                }
                                break FollowUpMessage::steer(
                                    text,
                                    UserAttachments::default(),
                                    id,
                                )
                                .for_target(session_id);
                            }
                            Ok(AppEvent::ExternalFollowUpRequested {
                                session_id,
                                text,
                                attachments,
                                follow_up_id,
                            }) if event_targets_external_session_or_side(
                                &Some(session_id.clone()),
                                &live_session_id,
                                &drain_config.alias_session_id,
                                &open_side_threads,
                            ) => {
                                let followup = FollowUpMessage::with_attachments(
                                    text,
                                    UserAttachments::from_items(attachments),
                                )
                                .for_target(Some(session_id))
                                .with_follow_up_id(follow_up_id);
                                if follow_up_message_was_cancelled(
                                    &mut cancelled_follow_ups,
                                    &followup,
                                ) {
                                    slog(&session_log, |l| {
                                        l.info("Skipped cancelled queued follow-up")
                                    });
                                    continue;
                                }
                                break followup;
                            }
                            Ok(AppEvent::CodexThreadActionRequested {
                                request_id,
                                session_id,
                                action,
                                params,
                                ..
                            }) if event_targets_external_session_or_side(
                                &session_id,
                                &live_session_id,
                                &drain_config.alias_session_id,
                                &open_side_threads,
                            ) => {
                                if !codex_thread_action_dedupe.mark_seen(&request_id) {
                                    continue;
                                }
                                if let Some(request) =
                                    external_context_rewind_request_from_action(
                                        &action,
                                        &params,
                                        session_id.clone(),
                                    )
                                {
                                    let request = match request {
                                        Ok(request) => request,
                                        Err(message) => {
                                            bus.send(AppEvent::CodexThreadActionResult {
                                                session_id: session_id.clone().or_else(|| live_session_id.clone()),
                                                action,
                                                success: false,
                                                message,
                                                record_id: None,
                                            });
                                            continue;
                                        }
                                    };
                                    if session_id
                                        .as_deref()
                                        .is_some_and(|id| open_side_threads.contains_key(id))
                                    {
                                        emit_context_rewind_failure(
                                            &request,
                                            "context rewind is not supported for side conversations".to_string(),
                                            &drain_config,
                                        );
                                        continue;
                                    }
                                    match apply_external_context_rewind(
                                        &mut agent,
                                        &thread.thread_id,
                                        &request,
                                        &drain_config,
                                    )
                                    .await
                                    {
                                        Ok(Some(followup)) => {
                                            break followup;
                                        }
                                        Ok(None) => {
                                            continue;
                                        }
                                        Err(message) => {
                                            emit_context_rewind_failure(
                                                &request,
                                                message,
                                                &drain_config,
                                            );
                                            continue;
                                        }
                                    }
                                }
                                if let Some(side_thread_id) = session_id
                                    .as_deref()
                                    .filter(|id| open_side_threads.contains_key(*id))
                                    .map(str::to_string)
                                {
                                    if action == "undo" {
                                        handle_side_undo_thread_action(
                                            &mut agent,
                                            &mut side_rounds,
                                            &mut side_turn_revisions,
                                            &side_thread_id,
                                            params,
                                            &drain_config,
                                        )
                                        .await;
                                        continue;
                                    }
                                }
                                if action == "undo" {
                                    handle_parent_undo_thread_action(
                                        &mut agent,
                                        &mut round,
                                        &mut user_turn_revisions,
                                        params,
                                        &drain_config,
                                    )
                                    .await;
                                    continue;
                                }
                                let effect = handle_external_thread_action(
                                    &mut agent,
                                    action,
                                    params,
                                    session_id,
                                    &drain_config,
                                )
                                .await;
                                if let ExternalThreadActionEffect::SideTurnStarted {
                                    parent_thread_id,
                                    child_thread_id,
                                    prompt,
                                } = effect
                                {
                                    open_side_threads.insert(
                                        child_thread_id.clone(),
                                        parent_thread_id.clone(),
                                    );
                                    side_rounds.entry(child_thread_id.clone()).or_insert(1);
                                    side_turn_revisions
                                        .entry(child_thread_id.clone())
                                        .or_insert_with(|| {
                                            let mut state = UserTurnRevisionState::default();
                                            state.record_next_turn();
                                            state
                                        });
                                    emit_side_session_started(
                                        &drain_config,
                                        &parent_thread_id,
                                        &child_thread_id,
                                        prompt.as_deref(),
                                    );
                                    drain_external_child_turn(
                                        &mut agent,
                                        &mut event_rx,
                                        &mut external_control_rx,
                                        &drain_config,
                                        &mut stats,
                                        &mut diff_tracker,
                                        &mut pending_runtime_steers,
                                        &mut handled_steer_ids,
                                        &mut cancelled_follow_ups,
                                        &mut codex_thread_action_dedupe,
                                        child_thread_id,
                                        "side",
                                    )
                                    .await;
                                } else if let ExternalThreadActionEffect::SideTurnClosed {
                                    child_thread_id,
                                } = effect
                                {
                                    open_side_threads.remove(&child_thread_id);
                                    side_rounds.remove(&child_thread_id);
                                    side_turn_revisions.remove(&child_thread_id);
                                }
                            }
                            Ok(AppEvent::InterruptRequested { session_id })
                                if event_targets_external_session_or_side(
                                    &session_id,
                                    &live_session_id,
                                    &drain_config.alias_session_id,
                                    &open_side_threads,
                                ) =>
                            {
                                // Ignore idle interrupts; this shared receiver
                                // consumed the event, so the next task will not
                                // inherit a stale Stop request.
                            }
                            Ok(_) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                slog(&session_log, |l| l.info("Event bus closed, exiting"));
                                stats.terminal_outcome = Some("event bus closed".to_string());
                                break 'outer;
                            }
                        }
                    }
                }
            },
        };
        if follow_up_message_was_cancelled(&mut cancelled_follow_ups, &followup) {
            slog(&session_log, |l| {
                l.info("Skipped cancelled queued follow-up")
            });
            continue;
        }
        let active_followup_for_rewind_replay = followup.clone();
        let turn_text = followup.text;
        let attachments = followup.attachments;
        let steer_id = followup.steer_id;
        let follow_up_id = followup.follow_up_id;
        let edit_user_turn_index = followup.edit_user_turn_index;
        let edit_user_turn_revision = followup.edit_user_turn_revision;
        let edit_original_text = followup.edit_original_text;
        let unresolved_attachment_ids = followup.unresolved_attachment_ids;
        let target_session_id = followup.target_session_id.clone();
        let managed_context_recovery_kickstart = followup.managed_context_recovery_kickstart;
        let managed_context_density_handoff = followup.managed_context_density_handoff;
        let managed_context_density_handoff_completed =
            followup.managed_context_density_handoff_completed;

        if let Some(side_thread_id) = target_session_id
            .as_deref()
            .filter(|id| open_side_threads.contains_key(*id))
            .map(str::to_string)
        {
            let mut replacement_for_user_turn_index = None;
            if let Some(user_turn_index) = edit_user_turn_index {
                if !agent.supports_user_message_rewind() {
                    let message = format!("{} does not support user-message rewind", agent.name());
                    slog(&session_log, |l| l.warn(&message));
                    bus.send(AppEvent::LoopError(message));
                    continue;
                }
                let current_side_round = *side_rounds.entry(side_thread_id.clone()).or_insert(1);
                let revisions = side_turn_revisions
                    .entry(side_thread_id.clone())
                    .or_default();
                revisions.seed_active_turns_to(current_side_round as u32);
                if let Err(message) =
                    revisions.validate_expected_revision(user_turn_index, edit_user_turn_revision)
                {
                    slog(&session_log, |l| l.warn(&message));
                    bus.send(AppEvent::LoopError(message));
                    continue;
                }
                match rollback_side_thread_from_turn(
                    &mut agent,
                    &mut side_rounds,
                    &mut side_turn_revisions,
                    &side_thread_id,
                    user_turn_index,
                    &drain_config,
                )
                .await
                {
                    Ok(turns_to_drop) => {
                        replacement_for_user_turn_index = Some(user_turn_index);
                        let message = format!(
                            "Edited side user turn {}; rolled back {} turn{}",
                            user_turn_index,
                            turns_to_drop,
                            if turns_to_drop == 1 { "" } else { "s" }
                        );
                        slog(&session_log, |l| l.info(&message));
                    }
                    Err(message) => {
                        slog(&session_log, |l| l.warn(&message));
                        bus.send(AppEvent::LoopError(message));
                        continue;
                    }
                }
            }

            let side_round = side_rounds.entry(side_thread_id.clone()).or_insert(0);
            *side_round += 1;
            let user_turn_revision = side_turn_revisions
                .entry(side_thread_id.clone())
                .or_default()
                .record_active_turn(*side_round as u32);
            emit_user_message_log(
                &bus,
                &session_log,
                Some(&side_thread_id),
                Some(*side_round as u32),
                Some(user_turn_revision),
                replacement_for_user_turn_index,
                &turn_text,
            );
            let merged = drain_steer_queue_as_followup(
                &context_injection,
                &turn_text,
                &bus,
                Some(&side_thread_id),
                None,
            )
            .unwrap_or_else(|| turn_text.clone());
            let side_thread = external_agent::AgentThread {
                thread_id: side_thread_id.clone(),
            };
            emit_external_turn_status(
                &bus,
                &autonomy,
                Some(&side_thread_id),
                *side_round,
                "thinking",
                format!("{} side turn in progress", agent.name()),
            )
            .await;
            let send_result = if attachments.is_empty() {
                agent.send_message(&side_thread, &merged).await
            } else {
                agent
                    .send_message_with_attachments(&side_thread, &merged, &attachments.items)
                    .await
            };
            if let Err(e) = send_result {
                emit_follow_up_status(
                    &bus,
                    Some(&side_thread_id),
                    &follow_up_id,
                    Some(&turn_text),
                    "failed",
                    Some("failed to send side follow-up"),
                );
                bus.send(AppEvent::LoopError(format!(
                    "Failed to send side follow-up: {}",
                    e
                )));
                continue;
            }
            emit_follow_up_status(
                &bus,
                Some(&side_thread_id),
                &follow_up_id,
                Some(&turn_text),
                "delivered",
                None,
            );
            if let Some(id) = steer_id {
                bus.send(AppEvent::SteerDelivered {
                    session_id: Some(side_thread_id.clone()),
                    id,
                    mid_turn: false,
                });
            }
            let parent_thread_id = open_side_threads.get(&side_thread_id).cloned();
            drain_external_child_turn(
                &mut agent,
                &mut event_rx,
                &mut external_control_rx,
                &drain_config,
                &mut stats,
                &mut diff_tracker,
                &mut pending_runtime_steers,
                &mut handled_steer_ids,
                &mut cancelled_follow_ups,
                &mut codex_thread_action_dedupe,
                side_thread_id,
                "side",
            )
            .await;
            if let Some(parent_thread_id) = parent_thread_id {
                if let Err(e) = agent.activate_thread(&parent_thread_id).await {
                    let message = format!("Failed to restore Codex parent thread: {}", e);
                    slog(&session_log, |l| l.warn(&message));
                    bus.send(AppEvent::LoopError(message));
                }
            }
            continue;
        }

        if let Some(subagent_thread_id) = target_session_id
            .as_deref()
            .filter(|id| stats.codex_subagent_parent_threads.contains_key(*id))
            .map(str::to_string)
        {
            if edit_user_turn_index.is_some() {
                let message = format!(
                    "User-message rewind is not supported for Codex subagent session {}",
                    subagent_thread_id.chars().take(8).collect::<String>()
                );
                slog(&session_log, |l| l.warn(&message));
                bus.send(AppEvent::LoopError(message));
                continue;
            }

            let subagent_round = stats
                .codex_subagent_rounds
                .entry(subagent_thread_id.clone())
                .or_insert(0);
            *subagent_round += 1;
            emit_user_message_log(
                &bus,
                &session_log,
                Some(&subagent_thread_id),
                Some(*subagent_round as u32),
                None,
                None,
                &turn_text,
            );
            let merged = drain_steer_queue_as_followup(
                &context_injection,
                &turn_text,
                &bus,
                Some(&subagent_thread_id),
                None,
            )
            .unwrap_or_else(|| turn_text.clone());
            let subagent_thread = external_agent::AgentThread {
                thread_id: subagent_thread_id.clone(),
            };
            let parent_thread_id = stats
                .codex_subagent_parent_threads
                .get(&subagent_thread_id)
                .cloned()
                .unwrap_or_else(|| thread.thread_id.clone());
            emit_external_turn_status(
                &bus,
                &autonomy,
                Some(&subagent_thread_id),
                *subagent_round,
                "thinking",
                format!("{} subagent turn in progress", agent.name()),
            )
            .await;
            let send_result = if attachments.is_empty() {
                agent.send_message(&subagent_thread, &merged).await
            } else {
                agent
                    .send_message_with_attachments(&subagent_thread, &merged, &attachments.items)
                    .await
            };
            if let Err(e) = send_result {
                let _ = agent.activate_thread(&parent_thread_id).await;
                emit_follow_up_status(
                    &bus,
                    Some(&subagent_thread_id),
                    &follow_up_id,
                    Some(&turn_text),
                    "failed",
                    Some("failed to send subagent follow-up"),
                );
                bus.send(AppEvent::LoopError(format!(
                    "Failed to send subagent follow-up: {}",
                    e
                )));
                continue;
            }
            emit_follow_up_status(
                &bus,
                Some(&subagent_thread_id),
                &follow_up_id,
                Some(&turn_text),
                "delivered",
                None,
            );
            if let Some(id) = steer_id {
                bus.send(AppEvent::SteerDelivered {
                    session_id: Some(subagent_thread_id.clone()),
                    id,
                    mid_turn: false,
                });
            }
            drain_external_child_turn(
                &mut agent,
                &mut event_rx,
                &mut external_control_rx,
                &drain_config,
                &mut stats,
                &mut diff_tracker,
                &mut pending_runtime_steers,
                &mut handled_steer_ids,
                &mut cancelled_follow_ups,
                &mut codex_thread_action_dedupe,
                subagent_thread_id,
                "subagent",
            )
            .await;
            if let Err(e) = agent.activate_thread(&parent_thread_id).await {
                let message = format!("Failed to restore Codex parent thread: {}", e);
                slog(&session_log, |l| l.warn(&message));
                bus.send(AppEvent::LoopError(message));
            }
            continue;
        }

        let managed_context_rewind_only_preflight_enabled =
            managed_context_preflight_rewind_only_gate_enabled(
                codex_managed_context_enabled,
                managed_context_recovery_kickstart,
                managed_context_density_handoff,
            );
        if managed_context_rewind_only_preflight_enabled {
            match refresh_external_context_usage_snapshot_for_preflight(&mut agent, &drain_config)
                .await
            {
                Ok(Some(snapshot)) => {
                    if let Some(pressure) = managed_context_rewind_only_pressure(&snapshot) {
                        let drop_original = managed_context_drop_original_for_recovery(
                            &turn_text,
                            !attachments.is_empty(),
                            steer_id.is_some(),
                            edit_user_turn_index.is_some(),
                        );
                        let held_user_input = !drop_original;
                        if held_user_input {
                            pending_managed_context_replays.push_back(FollowUpMessage {
                                text: turn_text.clone(),
                                attachments: attachments.clone(),
                                steer_id: steer_id.clone(),
                                follow_up_id: follow_up_id.clone(),
                                edit_user_turn_index,
                                edit_user_turn_revision,
                                edit_original_text: edit_original_text.clone(),
                                unresolved_attachment_ids: unresolved_attachment_ids.clone(),
                                target_session_id: target_session_id.clone(),
                                managed_context_recovery_kickstart: false,
                                managed_context_density_handoff: false,
                                managed_context_density_handoff_completed: false,
                            });
                            emit_follow_up_status(
                                &bus,
                                live_session_id.as_deref(),
                                &follow_up_id,
                                None,
                                "queued",
                                Some(
                                    "managed context is above the rewind-only threshold; recovering before sending this follow-up",
                                ),
                            );
                        } else {
                            emit_follow_up_status(
                                &bus,
                                live_session_id.as_deref(),
                                &follow_up_id,
                                Some(&turn_text),
                                "queued",
                                Some(
                                    "managed context is above the rewind-only threshold; treating this as a recovery kickstart",
                                ),
                            );
                        }

                        let recovery_text =
                            managed_context_recovery_kickstart_text(pressure, held_user_input);
                        slog(&session_log, |l| {
                            l.info(&format!(
                                "Holding Codex follow-up during managed-context {} pressure ({}/{} tokens); sending recovery kickstart",
                                pressure.status,
                                pressure.used_tokens,
                                pressure.rewind_only_limit
                            ))
                        });
                        bus.send(AppEvent::LogEntry {
                            session_id: live_session_id.clone(),
                            level: "info".to_string(),
                            source: "Intendant".to_string(),
                            content: format!(
                                "Managed context is in rewind-only pressure ({}/{} tokens); {}.",
                                pressure.used_tokens,
                                pressure.rewind_only_limit,
                                if held_user_input {
                                    "holding the user follow-up until recovery succeeds"
                                } else {
                                    "using the request as a recovery kickstart"
                                }
                            ),
                            turn: None,
                        });
                        let mut recovery_followup = FollowUpMessage::text(recovery_text)
                            .managed_context_recovery_kickstart();
                        if !held_user_input {
                            recovery_followup =
                                recovery_followup.with_follow_up_id(follow_up_id.clone());
                        }
                        next_turn = Some(recovery_followup);
                        continue 'outer;
                    } else if managed_context_preflight_density_gate_enabled(
                        managed_context_rewind_only_preflight_enabled,
                        managed_context_density_handoff_completed,
                    ) {
                        if let Some(pressure) = managed_context_density_pressure(&snapshot) {
                            pending_managed_context_replays.push_back(FollowUpMessage {
                                text: turn_text.clone(),
                                attachments: attachments.clone(),
                                steer_id: steer_id.clone(),
                                follow_up_id: follow_up_id.clone(),
                                edit_user_turn_index,
                                edit_user_turn_revision,
                                edit_original_text: edit_original_text.clone(),
                                unresolved_attachment_ids: unresolved_attachment_ids.clone(),
                                target_session_id: target_session_id.clone(),
                                managed_context_recovery_kickstart: false,
                                managed_context_density_handoff: false,
                                managed_context_density_handoff_completed: false,
                            });
                            emit_follow_up_status(
                                &bus,
                                live_session_id.as_deref(),
                                &follow_up_id,
                                None,
                                "queued",
                                Some(
                                    "managed context is above the recommended density threshold; sending density handoff before broad follow-up",
                                ),
                            );
                            let handoff_text = managed_context_density_handoff_text(pressure);
                            slog(&session_log, |l| {
                                l.info(&format!(
                                    "Holding Codex follow-up during managed-context density watch ({}/{} tokens, threshold {}); sending density handoff",
                                    pressure.used_tokens,
                                    pressure.rewind_only_limit,
                                    pressure.recommended_rewind_limit
                                ))
                            });
                            bus.send(AppEvent::LogEntry {
                                session_id: live_session_id.clone(),
                                level: "info".to_string(),
                                source: "Intendant".to_string(),
                                content: format!(
                                    "Managed context is above the recommended density threshold ({}/{} tokens, threshold {}). Sending a density handoff before broad follow-up work. Normal tools remain allowed below rewind-only pressure.",
                                    pressure.used_tokens,
                                    pressure.rewind_only_limit,
                                    pressure.recommended_rewind_limit
                                ),
                                turn: None,
                            });
                            next_turn = Some(
                                FollowUpMessage::text(handoff_text)
                                    .managed_context_density_handoff(),
                            );
                            continue 'outer;
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    slog(&session_log, |l| {
                        l.debug(&format!(
                            "Could not read Codex context snapshot before follow-up gate: {}",
                            e
                        ))
                    });
                }
            }
        }

        let mut replacement_for_user_turn_index = None;
        if let Some(user_turn_index) = edit_user_turn_index {
            bus.send(AppEvent::UserMessageEditStatus {
                session_id: live_session_id.clone(),
                user_turn_index,
                status: "running".to_string(),
                message: format!("applying edit to user turn {}", user_turn_index),
            });
            if !agent.supports_user_message_rewind() {
                let message = format!("{} does not support user-message rewind", agent.name());
                bus.send(AppEvent::UserMessageEditStatus {
                    session_id: live_session_id.clone(),
                    user_turn_index,
                    status: "failed".to_string(),
                    message: message.clone(),
                });
                emit_external_session_loop_error(
                    &bus,
                    &session_log,
                    live_session_id.as_deref(),
                    agent.name(),
                    message,
                );
                continue;
            }
            if user_turn_index == 0 {
                let message = format!(
                    "Cannot edit user turn 0 in {} session {}",
                    backend,
                    live_session_id
                        .as_deref()
                        .map(|sid| sid.chars().take(8).collect::<String>())
                        .unwrap_or_else(|| "unknown".to_string())
                );
                bus.send(AppEvent::UserMessageEditStatus {
                    session_id: live_session_id.clone(),
                    user_turn_index,
                    status: "failed".to_string(),
                    message: message.clone(),
                });
                emit_external_session_loop_error(
                    &bus,
                    &session_log,
                    live_session_id.as_deref(),
                    &backend.to_string(),
                    message,
                );
                continue;
            }
            let active_edit_revision_ok = user_turn_index as usize <= round
                && user_turn_revisions
                    .validate_expected_revision(user_turn_index, edit_user_turn_revision)
                    .is_ok();
            let mut archived_edit_branch_not_found = false;
            if !active_edit_revision_ok && codex_managed_context_enabled {
                match fork_managed_context_edit_branch(
                    &mut agent,
                    &thread.thread_id,
                    user_turn_index,
                    edit_original_text.as_deref(),
                    turn_text.clone(),
                    unresolved_attachment_ids.clone(),
                    &drain_config,
                )
                .await
                {
                    Ok(Some(message)) => {
                        slog(&session_log, |l| l.info(&message));
                        bus.send(AppEvent::CodexThreadActionResult {
                            session_id: live_session_id.clone(),
                            action: "managed-edit-branch".to_string(),
                            success: true,
                            message: message.clone(),
                            record_id: None,
                        });
                        emit_follow_up_status(
                            &bus,
                            live_session_id.as_deref(),
                            &follow_up_id,
                            Some(&turn_text),
                            "queued",
                            Some("created managed edit branch from archived context"),
                        );
                        bus.send(AppEvent::UserMessageEditStatus {
                            session_id: live_session_id.clone(),
                            user_turn_index,
                            status: "ok".to_string(),
                            message,
                        });
                        continue 'outer;
                    }
                    Ok(None) => {
                        archived_edit_branch_not_found = true;
                    }
                    Err(message) => {
                        bus.send(AppEvent::UserMessageEditStatus {
                            session_id: live_session_id.clone(),
                            user_turn_index,
                            status: "failed".to_string(),
                            message: message.clone(),
                        });
                        emit_external_session_loop_error(
                            &bus,
                            &session_log,
                            live_session_id.as_deref(),
                            &backend.to_string(),
                            message,
                        );
                        continue;
                    }
                }
            }
            if user_turn_index as usize > round {
                let message = format!(
                    "Cannot edit user turn {} in {} session {}; current user turn count is {}",
                    user_turn_index,
                    backend,
                    live_session_id
                        .as_deref()
                        .map(|sid| sid.chars().take(8).collect::<String>())
                        .unwrap_or_else(|| "unknown".to_string()),
                    round
                );
                bus.send(AppEvent::UserMessageEditStatus {
                    session_id: live_session_id.clone(),
                    user_turn_index,
                    status: "failed".to_string(),
                    message: message.clone(),
                });
                emit_external_session_loop_error(
                    &bus,
                    &session_log,
                    live_session_id.as_deref(),
                    &backend.to_string(),
                    message,
                );
                continue;
            }
            if let Err(message) = user_turn_revisions
                .validate_expected_revision(user_turn_index, edit_user_turn_revision)
            {
                let message = if archived_edit_branch_not_found {
                    format!(
                        "{message}. No matching managed-context archive was found for the clicked message text; the selected turn is no longer active and cannot be safely edited from this attach wrapper."
                    )
                } else {
                    message
                };
                bus.send(AppEvent::UserMessageEditStatus {
                    session_id: live_session_id.clone(),
                    user_turn_index,
                    status: "failed".to_string(),
                    message: message.clone(),
                });
                emit_external_session_loop_error(
                    &bus,
                    &session_log,
                    live_session_id.as_deref(),
                    &backend.to_string(),
                    message,
                );
                continue;
            }
            let turns_to_drop = round as u32 - user_turn_index + 1;
            let mut rollback_result = agent.rollback_turns(turns_to_drop).await;
            if let Err(err) = rollback_result.as_ref() {
                if backend == external_agent::AgentBackend::Codex
                    && external_rollback_turn_in_progress(err)
                {
                    let message = format!(
                        "Codex still has a turn in progress; pausing autonomous goal work and waiting before editing user turn {}",
                        user_turn_index
                    );
                    slog(&session_log, |l| l.info(&message));
                    bus.send(AppEvent::LogEntry {
                        session_id: live_session_id.clone(),
                        level: "info".to_string(),
                        source: "Codex".to_string(),
                        content: message,
                        turn: None,
                    });
                    match agent.pause_autonomous_goal(&thread.thread_id).await {
                        Ok(result) => {
                            if let Some(goal) = result.goal {
                                emit_external_session_goal(
                                    &drain_config,
                                    live_session_id.clone(),
                                    Some(goal),
                                );
                            } else if result.goal_absent {
                                emit_external_session_goal(
                                    &drain_config,
                                    live_session_id.clone(),
                                    None,
                                );
                            }
                        }
                        Err(e) => {
                            slog(&session_log, |l| {
                                l.debug(&format!(
                                    "Could not pause Codex goal before edit rollback retry: {}",
                                    e
                                ))
                            });
                        }
                    }

                    let mut side_session_state = ExternalSideSessionState {
                        open_side_threads: &mut open_side_threads,
                        side_rounds: &mut side_rounds,
                        side_turn_revisions: &mut side_turn_revisions,
                    };
                    let drain_outcome = drain_external_agent_events(
                        &mut agent,
                        &mut event_rx,
                        &mut external_control_rx,
                        &drain_config,
                        &mut stats,
                        &mut diff_tracker,
                        &mut pending_runtime_steers,
                        &mut handled_steer_ids,
                        &mut cancelled_follow_ups,
                        &mut codex_thread_action_dedupe,
                        Some(&mut side_session_state),
                        false,
                        false,
                        false,
                    )
                    .await;
                    // A native id announced mid-turn (Claude Code's first
                    // turn) becomes the loop's primary address before the
                    // outcome is reported, so follow-up controls targeting
                    // the upgraded id match this conversation.
                    if let Some(native) = stats.announced_native_session_id.take() {
                        if backend.thread_id_is_canonical(&native) {
                            slog(&session_log, |l| {
                                l.info(&format!(
                                    "External session address upgraded to native id {}",
                                    short_external_session_id(&native)
                                ))
                            });
                            rotate_external_identity(
                                &native,
                                &mut live_session_id,
                                &mut drain_config,
                            );
                        }
                    }
                    match drain_outcome {
                        DrainOutcome::TurnCompleted {
                            message,
                            turns_in_round,
                        } => {
                            stats.rounds = round;
                            record_external_done_and_round_inline(
                                &session_log,
                                persist_model_responses_inline,
                                live_session_id.as_deref(),
                                message.as_deref(),
                                round,
                                turns_in_round,
                            );
                            bus.send(AppEvent::DoneSignal {
                                session_id: live_session_id.clone(),
                                message: message.clone(),
                            });
                            bus.send(AppEvent::RoundComplete {
                                session_id: live_session_id.clone(),
                                round,
                                turns_in_round,
                                native_message_count: None,
                            });
                        }
                        DrainOutcome::ContextRewindRequested {
                            request,
                            message,
                            turns_in_round,
                            ..
                        } => {
                            stats.rounds = round;
                            record_external_done_and_round_inline(
                                &session_log,
                                persist_model_responses_inline,
                                live_session_id.as_deref(),
                                message.as_deref(),
                                round,
                                turns_in_round,
                            );
                            bus.send(AppEvent::DoneSignal {
                                session_id: live_session_id.clone(),
                                message: message.clone(),
                            });
                            bus.send(AppEvent::RoundComplete {
                                session_id: live_session_id.clone(),
                                round,
                                turns_in_round,
                                native_message_count: None,
                            });
                            emit_context_rewind_failure(
                                &request,
                                "user edit superseded the pending context rewind".to_string(),
                                &drain_config,
                            );
                        }
                        DrainOutcome::Interrupted { reason } => {
                            stats.rounds = round;
                            slog(&session_log, |l| {
                                l.info(&format!(
                                    "External agent interrupted before edit rollback: {}",
                                    reason
                                ))
                            });
                            record_external_round_inline(
                                &session_log,
                                persist_model_responses_inline,
                                round,
                                stats.turns,
                            );
                            bus.send(AppEvent::RoundComplete {
                                session_id: live_session_id.clone(),
                                round,
                                turns_in_round: stats.turns,
                                native_message_count: None,
                            });
                        }
                        DrainOutcome::RecoveryRequired {
                            message,
                            recovery_hint,
                            ..
                        } => {
                            let message =
                                recovery_required_message(&message, recovery_hint.as_deref());
                            slog(&session_log, |l| l.warn(&message));
                            bus.send(AppEvent::LoopError(message));
                            continue;
                        }
                        DrainOutcome::Terminated { reason, exit_code } => {
                            let message = format!(
                                "{} terminated before edit rollback: {} (exit code: {:?})",
                                agent.name(),
                                reason,
                                exit_code
                            );
                            slog(&session_log, |l| l.warn(&message));
                            bus.send(AppEvent::LoopError(message));
                            continue;
                        }
                        DrainOutcome::ChannelClosed => {
                            let message =
                                "External agent event channel closed before edit rollback"
                                    .to_string();
                            slog(&session_log, |l| l.warn(&message));
                            bus.send(AppEvent::LoopError(message));
                            continue;
                        }
                    }
                    rollback_result = agent.rollback_turns(turns_to_drop).await;
                }
            }
            match rollback_result {
                Ok(()) => {
                    user_turn_revisions.rewind_from_turn(user_turn_index);
                    round = user_turn_index.saturating_sub(1) as usize;
                    replacement_for_user_turn_index = Some(user_turn_index);
                    let message = format!(
                        "Edited user turn {}; rolled back {} turn{}",
                        user_turn_index,
                        turns_to_drop,
                        if turns_to_drop == 1 { "" } else { "s" }
                    );
                    slog(&session_log, |l| l.info(&message));
                    bus.send(AppEvent::UserMessageRewind {
                        session_id: live_session_id.clone(),
                        user_turn_index,
                        turns_removed: turns_to_drop,
                    });
                    bus.send(AppEvent::UserMessageEditStatus {
                        session_id: live_session_id.clone(),
                        user_turn_index,
                        status: "ok".to_string(),
                        message,
                    });
                }
                Err(e) => {
                    let message = format!(
                        "Cannot edit user turn {} in {} session: {}",
                        user_turn_index, backend, e
                    );
                    bus.send(AppEvent::UserMessageEditStatus {
                        session_id: live_session_id.clone(),
                        user_turn_index,
                        status: "failed".to_string(),
                        message: message.clone(),
                    });
                    emit_external_session_loop_error(
                        &bus,
                        &session_log,
                        live_session_id.as_deref(),
                        &backend.to_string(),
                        message,
                    );
                    continue;
                }
            }
        }

        round += 1;
        let user_turn_revision = user_turn_revisions.record_active_turn(round as u32);
        stats.turns = 0;
        let attachment_count = attachments.len();
        let merged = drain_steer_queue_as_followup(
            &context_injection,
            &turn_text,
            &bus,
            live_session_id.as_deref(),
            drain_config.alias_session_id.as_deref(),
        )
        .unwrap_or_else(|| turn_text.clone());
        let user_log_text = if turn_text.trim().is_empty() {
            &merged
        } else {
            &turn_text
        };
        emit_user_message_log(
            &bus,
            &session_log,
            live_session_id.as_deref(),
            Some(round as u32),
            Some(user_turn_revision),
            replacement_for_user_turn_index,
            user_log_text,
        );
        slog(&session_log, |l| {
            if round == 1 {
                l.info(&format!(
                    "Initial task sent to external agent{}",
                    if attachment_count == 0 {
                        String::new()
                    } else {
                        format!(" with {} attachment(s)", attachment_count)
                    }
                ));
            } else {
                l.info(&format!(
                    "Follow-up round {}: {}{}",
                    round,
                    merged,
                    if attachment_count == 0 {
                        String::new()
                    } else {
                        format!(" ({} attachment(s))", attachment_count)
                    }
                ));
            }
        });
        diff_tracker.seed_from_session_log(&project.root, &log_dir);
        emit_external_turn_status(
            &bus,
            &autonomy,
            live_session_id.as_deref(),
            round,
            "thinking",
            external_turn_status_task(agent.name(), round, user_log_text),
        )
        .await;
        let send_result = if attachments.is_empty() {
            agent.send_message(&thread, &merged).await
        } else {
            agent
                .send_message_with_attachments(&thread, &merged, &attachments.items)
                .await
        };
        if let Err(e) = send_result {
            emit_follow_up_status(
                &bus,
                live_session_id.as_deref(),
                &follow_up_id,
                Some(&turn_text),
                "failed",
                Some("failed to send follow-up"),
            );
            if round == 1 {
                return Err(e);
            }
            bus.send(AppEvent::LoopError(format!(
                "Failed to send follow-up: {}",
                e
            )));
            stats.terminal_outcome = Some(format!("failed to send follow-up: {}", e));
            break;
        }
        emit_follow_up_status(
            &bus,
            live_session_id.as_deref(),
            &follow_up_id,
            Some(&turn_text),
            "delivered",
            None,
        );
        if let Some(id) = follow_up_id.as_deref() {
            // Pairs with the supervisor's "FollowUp … queued" daemon-log
            // line; queued without delivered means the queue stopped
            // draining.
            slog(&session_log, |l| {
                l.debug(&format!("Follow-up {} delivered to {}", id, agent.name()))
            });
        }
        if let Some(id) = steer_id {
            bus.send(AppEvent::SteerDelivered {
                session_id: live_session_id.clone(),
                id,
                mid_turn: false,
            });
        }

        let mut side_session_state = ExternalSideSessionState {
            open_side_threads: &mut open_side_threads,
            side_rounds: &mut side_rounds,
            side_turn_revisions: &mut side_turn_revisions,
        };
        let drain_outcome = drain_external_agent_events(
            &mut agent,
            &mut event_rx,
            &mut external_control_rx,
            &drain_config,
            &mut stats,
            &mut diff_tracker,
            &mut pending_runtime_steers,
            &mut handled_steer_ids,
            &mut cancelled_follow_ups,
            &mut codex_thread_action_dedupe,
            Some(&mut side_session_state),
            managed_context_recovery_kickstart,
            managed_context_density_handoff,
            managed_context_density_handoff_completed,
        )
        .await;
        // A native id announced mid-turn (Claude Code's first turn) becomes
        // the loop's primary address before the outcome is reported, so
        // targeted controls sent under the upgraded id keep matching.
        if let Some(native) = stats.announced_native_session_id.take() {
            if backend.thread_id_is_canonical(&native) {
                slog(&session_log, |l| {
                    l.info(&format!(
                        "External session address upgraded to native id {}",
                        short_external_session_id(&native)
                    ))
                });
                rotate_external_identity(&native, &mut live_session_id, &mut drain_config);
            }
        }
        match drain_outcome {
            DrainOutcome::TurnCompleted {
                message,
                turns_in_round,
            } => {
                stats.rounds = round;
                if codex_managed_context_enabled {
                    match refresh_external_context_usage_snapshot(&mut agent, &drain_config).await {
                        Ok(Some(snapshot)) => {
                            if let Some(pressure) = managed_context_rewind_only_pressure(&snapshot)
                            {
                                managed_context_recovery_kickstarts_without_rewind =
                                    managed_context_recovery_kickstarts_without_rewind
                                        .saturating_add(1);
                                if managed_context_recovery_kickstarts_without_rewind
                                    < MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND
                                {
                                    let held_user_input =
                                        !pending_managed_context_replays.is_empty();
                                    let recovery_text = managed_context_recovery_kickstart_text(
                                        pressure,
                                        held_user_input,
                                    );
                                    let turn_kind = if managed_context_recovery_kickstart {
                                        "recovery kickstart"
                                    } else {
                                        "managed Codex turn"
                                    };
                                    slog(&session_log, |l| {
                                        l.warn(&format!(
                                            "Managed-context {turn_kind} completed without a context rewind while pressure remains {}/{} tokens; retrying recovery",
                                            pressure.used_tokens,
                                            pressure.rewind_only_limit
                                        ))
                                    });
                                    bus.send(AppEvent::LogEntry {
                                        session_id: live_session_id.clone(),
                                        level: "warn".to_string(),
                                        source: "Intendant".to_string(),
                                        content: format!(
                                            "Managed-context {turn_kind} did not reduce context below the rewind-only threshold; context still reports {}/{} tokens. Retrying recovery before sending any normal follow-up.",
                                            pressure.used_tokens,
                                            pressure.rewind_only_limit
                                        ),
                                        turn: None,
                                    });
                                    record_external_round_inline(
                                        &session_log,
                                        persist_model_responses_inline,
                                        round,
                                        turns_in_round,
                                    );
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: live_session_id.clone(),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                    next_turn = Some(
                                        FollowUpMessage::text(recovery_text)
                                            .managed_context_recovery_kickstart(),
                                    );
                                    continue 'outer;
                                } else {
                                    // Model-driven recovery exhausted its retry
                                    // budget (the fork's recovery turn hit its
                                    // step limit each time without rewinding).
                                    // Backstop: supervisor-forced surgical
                                    // rewind instead of session death.
                                    let mut surgical_failure = None;
                                    if managed_context_surgical_recovery_available(
                                        managed_context_surgical_recoveries,
                                    ) {
                                        match attempt_supervisor_surgical_context_rewind(
                                            &mut agent,
                                            &thread.thread_id,
                                            &drain_config,
                                            surgical_task_statement.as_deref(),
                                            &mut pending_managed_context_replays,
                                        )
                                        .await
                                        {
                                            Ok(continuation) => {
                                                managed_context_surgical_recoveries =
                                                    managed_context_surgical_recoveries
                                                        .saturating_add(1);
                                                managed_context_recovery_kickstarts_without_rewind =
                                                    0;
                                                let content = format!(
                                                    "Managed-context recovery exhausted {} kickstarts without a rewind at {}/{} tokens; Intendant performed a surgical rewind ({} of {}) and is resuming the session.",
                                                    MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND,
                                                    pressure.used_tokens,
                                                    pressure.rewind_only_limit,
                                                    managed_context_surgical_recoveries,
                                                    MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES,
                                                );
                                                slog(&session_log, |l| l.warn(&content));
                                                bus.send(AppEvent::LogEntry {
                                                    session_id: live_session_id.clone(),
                                                    level: "warn".to_string(),
                                                    source: "Intendant".to_string(),
                                                    content,
                                                    turn: None,
                                                });
                                                record_external_round_inline(
                                                    &session_log,
                                                    persist_model_responses_inline,
                                                    round,
                                                    turns_in_round,
                                                );
                                                bus.send(AppEvent::RoundComplete {
                                                    session_id: live_session_id.clone(),
                                                    round,
                                                    turns_in_round,
                                                    native_message_count: None,
                                                });
                                                next_turn = Some(continuation);
                                                continue 'outer;
                                            }
                                            Err(e) => surgical_failure = Some(e),
                                        }
                                    }
                                    let mut message = format!(
                                        "Managed-context recovery completed without rewind_context while context remains above the rewind-only threshold ({}/{} tokens); refusing to send normal follow-ups.",
                                        pressure.used_tokens,
                                        pressure.rewind_only_limit
                                    );
                                    match surgical_failure {
                                        Some(failure) => {
                                            message.push_str(&format!(
                                                " Supervisor surgical rewind also failed: {failure}"
                                            ));
                                        }
                                        None => {
                                            message.push_str(&format!(
                                                " Supervisor surgical recovery budget ({} per session) is exhausted.",
                                                MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES
                                            ));
                                        }
                                    }
                                    slog(&session_log, |l| l.warn(&message));
                                    record_external_round_inline(
                                        &session_log,
                                        persist_model_responses_inline,
                                        round,
                                        turns_in_round,
                                    );
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: live_session_id.clone(),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                    bus.send(AppEvent::LoopError(message));
                                    stats.terminal_outcome = Some(
                                        "managed Codex context pressure unresolved".to_string(),
                                    );
                                    break;
                                }
                            } else {
                                managed_context_recovery_kickstarts_without_rewind = 0;
                                managed_context_density_block_handoffs_without_relief = 0;
                                if managed_context_recovery_without_rewind_blocks_held_replay(
                                    managed_context_recovery_kickstart,
                                    &pending_managed_context_replays,
                                ) {
                                    let message = "Managed-context recovery turn completed without rewind_context; refusing to replay held normal follow-up until a successful rewind lowers context pressure.".to_string();
                                    slog(&session_log, |l| l.warn(&message));
                                    record_external_round_inline(
                                        &session_log,
                                        persist_model_responses_inline,
                                        round,
                                        turns_in_round,
                                    );
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: live_session_id.clone(),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                    bus.send(AppEvent::LoopError(message));
                                    stats.terminal_outcome =
                                        Some("managed Codex recovery did not rewind".to_string());
                                    break;
                                }
                                if let Some(mut replay) =
                                    pending_managed_context_replays.pop_front()
                                {
                                    if managed_context_density_handoff {
                                        slog(&session_log, |l| {
                                            l.info(
                                                "Managed-context density handoff completed without a context rewind; replaying held follow-up",
                                            )
                                        });
                                        replay = replay.after_managed_context_density_handoff();
                                    } else {
                                        slog(&session_log, |l| {
                                            l.warn(
                                                "Managed-context pressure cleared without a context rewind; replaying held follow-up",
                                            )
                                        });
                                    }
                                    record_external_round_inline(
                                        &session_log,
                                        persist_model_responses_inline,
                                        round,
                                        turns_in_round,
                                    );
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: live_session_id.clone(),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                    next_turn = Some(replay);
                                    continue 'outer;
                                }
                                if managed_context_post_turn_density_handoff_enabled(
                                    managed_context_recovery_kickstart,
                                    managed_context_density_handoff,
                                    managed_context_density_handoff_completed,
                                ) {
                                    if let Some(pressure) =
                                        managed_context_density_pressure(&snapshot)
                                    {
                                        let handoff_text =
                                            managed_context_density_handoff_text(pressure);
                                        slog(&session_log, |l| {
                                            l.info(&format!(
                                                "Managed Codex completed at density-watch pressure ({}/{} tokens); sending one-shot context handoff before waiting for follow-up",
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit
                                            ))
                                        });
                                        bus.send(AppEvent::LogEntry {
                                            session_id: live_session_id.clone(),
                                            level: "info".to_string(),
                                            source: "Intendant".to_string(),
                                            content: format!(
                                                "Managed context is above the recommended density threshold ({}/{} tokens, threshold {}). Sending a one-shot context handoff before waiting for follow-up.",
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit,
                                                pressure.recommended_rewind_limit
                                            ),
                                            turn: None,
                                        });
                                        record_external_round_inline(
                                            &session_log,
                                            persist_model_responses_inline,
                                            round,
                                            turns_in_round,
                                        );
                                        bus.send(AppEvent::RoundComplete {
                                            session_id: live_session_id.clone(),
                                            round,
                                            turns_in_round,
                                            native_message_count: None,
                                        });
                                        next_turn = Some(
                                            FollowUpMessage::text(handoff_text)
                                                .managed_context_density_handoff(),
                                        );
                                        continue 'outer;
                                    }
                                }
                            }
                        }
                        Ok(None) => {
                            if managed_context_recovery_kickstart
                                || !pending_managed_context_replays.is_empty()
                            {
                                let message = "Managed-context recovery completed without rewind_context, and Codex context pressure could not be re-read; refusing to send normal follow-ups.".to_string();
                                slog(&session_log, |l| l.warn(&message));
                                record_external_round_inline(
                                    &session_log,
                                    persist_model_responses_inline,
                                    round,
                                    turns_in_round,
                                );
                                bus.send(AppEvent::RoundComplete {
                                    session_id: live_session_id.clone(),
                                    round,
                                    turns_in_round,
                                    native_message_count: None,
                                });
                                bus.send(AppEvent::LoopError(message));
                                stats.terminal_outcome =
                                    Some("managed Codex context pressure unreadable".to_string());
                                break;
                            }
                        }
                        Err(e) => {
                            if managed_context_recovery_kickstart
                                || !pending_managed_context_replays.is_empty()
                            {
                                let message = format!(
                                    "Managed-context recovery completed without rewind_context, and Codex context pressure could not be re-read: {}; refusing to send normal follow-ups.",
                                    e
                                );
                                slog(&session_log, |l| l.warn(&message));
                                record_external_round_inline(
                                    &session_log,
                                    persist_model_responses_inline,
                                    round,
                                    turns_in_round,
                                );
                                bus.send(AppEvent::RoundComplete {
                                    session_id: live_session_id.clone(),
                                    round,
                                    turns_in_round,
                                    native_message_count: None,
                                });
                                bus.send(AppEvent::LoopError(message));
                                stats.terminal_outcome =
                                    Some("managed Codex context pressure unreadable".to_string());
                                break;
                            } else {
                                slog(&session_log, |l| {
                                    l.debug(&format!(
                                        "Could not re-read Codex context pressure after managed turn: {}",
                                        e
                                    ))
                                });
                            }
                        }
                    }
                }

                record_external_done_and_round_inline(
                    &session_log,
                    persist_model_responses_inline,
                    live_session_id.as_deref(),
                    message.as_deref(),
                    round,
                    turns_in_round,
                );
                bus.send(AppEvent::DoneSignal {
                    session_id: live_session_id.clone(),
                    message: message.clone(),
                });
                bus.send(AppEvent::RoundComplete {
                    session_id: live_session_id.clone(),
                    round,
                    turns_in_round,
                    native_message_count: None,
                });
            }
            DrainOutcome::ContextRewindRequested {
                request,
                message,
                turns_in_round,
                turn_stop_status,
            } => {
                managed_context_recovery_kickstarts_without_rewind = 0;
                managed_context_density_block_handoffs_without_relief = 0;
                stats.rounds = round;
                match apply_external_context_rewind(
                    &mut agent,
                    &thread.thread_id,
                    &request,
                    &drain_config,
                )
                .await
                {
                    Ok(automatic_resume) => {
                        if let Some(mut continuation) = managed_context_rewind_continuation(
                            &mut pending_managed_context_replays,
                            &active_followup_for_rewind_replay,
                            automatic_resume,
                            &turn_stop_status,
                        ) {
                            if managed_context_density_handoff {
                                continuation = continuation.after_managed_context_density_handoff();
                            }
                            slog(&session_log, |l| {
                                l.info(
                                    "Managed-context rewind succeeded; continuing queued follow-up",
                                )
                            });
                            next_turn = Some(continuation);
                            continue 'outer;
                        }
                        record_external_done_and_round_inline(
                            &session_log,
                            persist_model_responses_inline,
                            live_session_id.as_deref(),
                            message.as_deref(),
                            round,
                            turns_in_round,
                        );
                        bus.send(AppEvent::DoneSignal {
                            session_id: live_session_id.clone(),
                            message: message.clone(),
                        });
                        bus.send(AppEvent::RoundComplete {
                            session_id: live_session_id.clone(),
                            round,
                            turns_in_round,
                            native_message_count: None,
                        });
                    }
                    Err(message) => {
                        emit_context_rewind_failure(&request, message, &drain_config);
                        record_external_done_and_round_inline(
                            &session_log,
                            persist_model_responses_inline,
                            live_session_id.as_deref(),
                            None,
                            round,
                            turns_in_round,
                        );
                        bus.send(AppEvent::DoneSignal {
                            session_id: live_session_id.clone(),
                            message: None,
                        });
                        bus.send(AppEvent::RoundComplete {
                            session_id: live_session_id.clone(),
                            round,
                            turns_in_round,
                            native_message_count: None,
                        });
                    }
                }
            }
            DrainOutcome::RecoveryRequired {
                message,
                recovery_hint,
                turns_in_round,
            } => {
                stats.rounds = round;
                if codex_managed_context_enabled {
                    managed_context_recovery_kickstarts_without_rewind =
                        managed_context_recovery_kickstarts_without_rewind.saturating_add(1);
                    if managed_context_recovery_kickstarts_without_rewind
                        < MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND
                    {
                        let pressure = match refresh_external_context_usage_snapshot(
                            &mut agent,
                            &drain_config,
                        )
                        .await
                        {
                            Ok(Some(snapshot)) => managed_context_recovery_pressure(&snapshot),
                            Ok(None) => None,
                            Err(e) => {
                                slog(&session_log, |l| {
                                    l.debug(&format!(
                                        "Could not read Codex context snapshot after recovery-required outcome: {}",
                                        e
                                    ))
                                });
                                None
                            }
                        };
                        let recovery_text = pressure
                            .map(|pressure| {
                                managed_context_recovery_kickstart_text(pressure, false)
                            })
                            .unwrap_or_else(|| {
                                managed_context_backend_recovery_kickstart_text(
                                    &message,
                                    recovery_hint.as_deref(),
                                )
                            });
                        slog(&session_log, |l| {
                            l.warn("Managed Codex reported recovery required; sending managed-context recovery kickstart instead of ending the session")
                        });
                        record_external_round_inline(
                            &session_log,
                            persist_model_responses_inline,
                            round,
                            turns_in_round,
                        );
                        bus.send(AppEvent::RoundComplete {
                            session_id: live_session_id.clone(),
                            round,
                            turns_in_round,
                            native_message_count: None,
                        });
                        bus.send(AppEvent::LogEntry {
                            session_id: live_session_id.clone(),
                            level: "warn".to_string(),
                            source: "Intendant".to_string(),
                            content: "Managed Codex reported recovery required; sending a managed-context rewind kickstart instead of ending the session.".to_string(),
                            turn: None,
                        });
                        next_turn = Some(
                            FollowUpMessage::text(recovery_text)
                                .managed_context_recovery_kickstart(),
                        );
                        continue 'outer;
                    } else {
                        // Backstop: the model kept reporting recovery required
                        // without rewinding (step-limit exhaustion ends those
                        // turns); perform a surgical rewind before giving up.
                        let mut surgical_failure = None;
                        if managed_context_surgical_recovery_available(
                            managed_context_surgical_recoveries,
                        ) {
                            match attempt_supervisor_surgical_context_rewind(
                                &mut agent,
                                &thread.thread_id,
                                &drain_config,
                                surgical_task_statement.as_deref(),
                                &mut pending_managed_context_replays,
                            )
                            .await
                            {
                                Ok(continuation) => {
                                    managed_context_surgical_recoveries =
                                        managed_context_surgical_recoveries.saturating_add(1);
                                    managed_context_recovery_kickstarts_without_rewind = 0;
                                    let content = format!(
                                        "Managed Codex kept reporting backend recovery required after {} kickstarts without a rewind; Intendant performed a surgical rewind ({} of {}) and is resuming the session.",
                                        MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND,
                                        managed_context_surgical_recoveries,
                                        MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES,
                                    );
                                    slog(&session_log, |l| l.warn(&content));
                                    bus.send(AppEvent::LogEntry {
                                        session_id: live_session_id.clone(),
                                        level: "warn".to_string(),
                                        source: "Intendant".to_string(),
                                        content,
                                        turn: None,
                                    });
                                    record_external_round_inline(
                                        &session_log,
                                        persist_model_responses_inline,
                                        round,
                                        turns_in_round,
                                    );
                                    bus.send(AppEvent::RoundComplete {
                                        session_id: live_session_id.clone(),
                                        round,
                                        turns_in_round,
                                        native_message_count: None,
                                    });
                                    next_turn = Some(continuation);
                                    continue 'outer;
                                }
                                Err(e) => surgical_failure = Some(e),
                            }
                        }
                        let mut failure = format!(
                            "Managed Codex still reports backend recovery required after {} recovery kickstarts without another successful rewind; refusing to mark the session complete.",
                            managed_context_recovery_kickstarts_without_rewind
                        );
                        match surgical_failure {
                            Some(surgical) => failure.push_str(&format!(
                                " Supervisor surgical rewind also failed: {surgical}"
                            )),
                            None => failure.push_str(&format!(
                                " Supervisor surgical recovery budget ({} per session) is exhausted.",
                                MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES
                            )),
                        }
                        slog(&session_log, |l| l.warn(&failure));
                        record_external_round_inline(
                            &session_log,
                            persist_model_responses_inline,
                            round,
                            turns_in_round,
                        );
                        bus.send(AppEvent::RoundComplete {
                            session_id: live_session_id.clone(),
                            round,
                            turns_in_round,
                            native_message_count: None,
                        });
                        bus.send(AppEvent::LogEntry {
                            session_id: live_session_id.clone(),
                            level: "error".to_string(),
                            source: "Intendant".to_string(),
                            content: failure.clone(),
                            turn: None,
                        });
                        bus.send(AppEvent::LoopError(failure));
                        stats.terminal_outcome =
                            Some("managed Codex recovery required".to_string());
                        break;
                    }
                }
                slog(&session_log, |l| {
                    l.warn(&recovery_required_message(
                        &message,
                        recovery_hint.as_deref(),
                    ))
                });
                record_external_round_inline(
                    &session_log,
                    persist_model_responses_inline,
                    round,
                    turns_in_round,
                );
                bus.send(AppEvent::RoundComplete {
                    session_id: live_session_id.clone(),
                    round,
                    turns_in_round,
                    native_message_count: None,
                });
                bus.send(AppEvent::TaskComplete {
                    session_id: live_session_id.clone(),
                    reason: "recovery required".to_string(),
                    summary: recovery_hint.or(Some(message)),
                });
                stats.terminal_outcome = Some("recovery required".to_string());
                break;
            }
            DrainOutcome::Interrupted { reason } => {
                // Emit RoundComplete so the dashboard updates and log the
                // interrupt. For a *user-requested* interrupt the round ends
                // here and the loop waits for the next follow-up. When the
                // managed-context density tool gate generated the interrupt,
                // there may be no user at all (headless `--task-file` runs),
                // so the supervisor must continue the loop itself with the
                // density maintenance handoff (managed.md: density gating
                // inserts a maintenance handoff) or a recovery kickstart if
                // pressure escalated past the rewind-only threshold.
                stats.rounds = round;
                slog(&session_log, |l| {
                    l.info(&format!("External agent interrupted: {}", reason))
                });
                record_external_round_inline(
                    &session_log,
                    persist_model_responses_inline,
                    round,
                    stats.turns,
                );
                bus.send(AppEvent::RoundComplete {
                    session_id: live_session_id.clone(),
                    round,
                    turns_in_round: stats.turns,
                    native_message_count: None,
                });
                if codex_managed_context_enabled
                    && reason == MANAGED_CONTEXT_DENSITY_BLOCK_INTERRUPT_REASON
                {
                    match refresh_external_context_usage_snapshot(&mut agent, &drain_config).await {
                        Ok(Some(snapshot)) => {
                            if let Some(pressure) = managed_context_rewind_only_pressure(&snapshot)
                            {
                                managed_context_recovery_kickstarts_without_rewind =
                                    managed_context_recovery_kickstarts_without_rewind
                                        .saturating_add(1);
                                if managed_context_recovery_kickstarts_without_rewind
                                    < MANAGED_CONTEXT_RECOVERY_MAX_KICKSTARTS_WITHOUT_REWIND
                                {
                                    let held_user_input =
                                        !pending_managed_context_replays.is_empty();
                                    let recovery_text = managed_context_recovery_kickstart_text(
                                        pressure,
                                        held_user_input,
                                    );
                                    slog(&session_log, |l| {
                                        l.warn(&format!(
                                            "Managed-context density tool gate interrupted the turn while pressure escalated to rewind-only ({}/{} tokens); sending recovery kickstart",
                                            pressure.used_tokens,
                                            pressure.rewind_only_limit
                                        ))
                                    });
                                    next_turn = Some(
                                        FollowUpMessage::text(recovery_text)
                                            .managed_context_recovery_kickstart(),
                                    );
                                    continue 'outer;
                                }
                                // Backstop: surgical rewind before giving up
                                // (same exhaustion as the TurnCompleted arm,
                                // reached via the density-gate interrupt).
                                let mut surgical_failure = None;
                                if managed_context_surgical_recovery_available(
                                    managed_context_surgical_recoveries,
                                ) {
                                    match attempt_supervisor_surgical_context_rewind(
                                        &mut agent,
                                        &thread.thread_id,
                                        &drain_config,
                                        surgical_task_statement.as_deref(),
                                        &mut pending_managed_context_replays,
                                    )
                                    .await
                                    {
                                        Ok(continuation) => {
                                            managed_context_surgical_recoveries =
                                                managed_context_surgical_recoveries
                                                    .saturating_add(1);
                                            managed_context_recovery_kickstarts_without_rewind = 0;
                                            let content = format!(
                                                "Managed-context recovery exhausted its kickstart budget at {}/{} tokens; Intendant performed a surgical rewind ({} of {}) and is resuming the session.",
                                                pressure.used_tokens,
                                                pressure.rewind_only_limit,
                                                managed_context_surgical_recoveries,
                                                MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES,
                                            );
                                            slog(&session_log, |l| l.warn(&content));
                                            bus.send(AppEvent::LogEntry {
                                                session_id: live_session_id.clone(),
                                                level: "warn".to_string(),
                                                source: "Intendant".to_string(),
                                                content,
                                                turn: None,
                                            });
                                            next_turn = Some(continuation);
                                            continue 'outer;
                                        }
                                        Err(e) => surgical_failure = Some(e),
                                    }
                                }
                                let mut message = format!(
                                    "Managed-context density tool gate kept interrupting while context stayed above the rewind-only threshold ({}/{} tokens); refusing to continue without a rewind.",
                                    pressure.used_tokens, pressure.rewind_only_limit
                                );
                                match surgical_failure {
                                    Some(failure) => message.push_str(&format!(
                                        " Supervisor surgical rewind also failed: {failure}"
                                    )),
                                    None => message.push_str(&format!(
                                        " Supervisor surgical recovery budget ({} per session) is exhausted.",
                                        MANAGED_CONTEXT_MAX_SURGICAL_RECOVERIES
                                    )),
                                }
                                slog(&session_log, |l| l.warn(&message));
                                bus.send(AppEvent::LoopError(message));
                                stats.terminal_outcome =
                                    Some("managed Codex context pressure unresolved".to_string());
                                break;
                            }
                            if let Some(pressure) = managed_context_density_pressure(&snapshot) {
                                managed_context_density_block_handoffs_without_relief =
                                    managed_context_density_block_handoffs_without_relief
                                        .saturating_add(1);
                                if managed_context_density_block_handoffs_without_relief
                                    < MANAGED_CONTEXT_DENSITY_BLOCK_MAX_HANDOFFS_WITHOUT_RELIEF
                                {
                                    let handoff_text =
                                        managed_context_density_handoff_text(pressure);
                                    slog(&session_log, |l| {
                                        l.info(&format!(
                                            "Managed-context density tool gate interrupted the turn ({}/{} tokens, threshold {}); sending density maintenance handoff",
                                            pressure.used_tokens,
                                            pressure.rewind_only_limit,
                                            pressure.recommended_rewind_limit
                                        ))
                                    });
                                    next_turn = Some(
                                        FollowUpMessage::text(handoff_text)
                                            .managed_context_density_handoff(),
                                    );
                                    continue 'outer;
                                }
                                let message = format!(
                                    "Managed-context density maintenance did not converge after {} handoffs ({}/{} tokens, threshold {}); refusing to ping-pong until the task timeout.",
                                    managed_context_density_block_handoffs_without_relief,
                                    pressure.used_tokens,
                                    pressure.rewind_only_limit,
                                    pressure.recommended_rewind_limit
                                );
                                slog(&session_log, |l| l.warn(&message));
                                bus.send(AppEvent::LoopError(message));
                                stats.terminal_outcome = Some(
                                    "managed Codex density maintenance unresolved".to_string(),
                                );
                                break;
                            }
                            // Pressure dropped below the density threshold
                            // between the block and this re-read (a fresher
                            // backend report landed); the steer is stale —
                            // resume the interrupted task.
                            managed_context_density_block_handoffs_without_relief = 0;
                            slog(&session_log, |l| {
                                l.info(
                                    "Managed-context density tool gate interrupted the turn, but a fresher backend report is below the density threshold; resuming the task",
                                )
                            });
                            next_turn = Some(FollowUpMessage::text(
                                "The previous turn was interrupted by a managed-context density gate, but the latest backend report now shows context pressure below the recommended density threshold, so that steer is stale. Continue the task from where it was interrupted."
                                    .to_string(),
                            ));
                            continue 'outer;
                        }
                        Ok(None) => {
                            slog(&session_log, |l| {
                                l.warn(
                                    "Managed-context density tool gate interrupted the turn, but no backend context report is available; waiting for a follow-up",
                                )
                            });
                        }
                        Err(e) => {
                            slog(&session_log, |l| {
                                l.warn(&format!(
                                    "Managed-context density tool gate interrupted the turn, but context pressure could not be re-read: {}; waiting for a follow-up",
                                    e
                                ))
                            });
                        }
                    }
                }
            }
            DrainOutcome::Terminated { reason, exit_code } => {
                stats.rounds = round;
                let user_requested_stop =
                    matches!(reason.as_str(), "stopped by user" | "restarting session");
                if codex_managed_context_enabled && !user_requested_stop {
                    match refresh_external_context_usage_snapshot(&mut agent, &drain_config).await {
                        Ok(Some(snapshot)) => {
                            if let Some(pressure) = managed_context_rewind_only_pressure(&snapshot)
                            {
                                let message = format!(
                                    "Managed Codex terminated as {reason} while backend-reported pressure remains {}/{} tokens; refusing to mark the session complete.",
                                    pressure.used_tokens,
                                    pressure.rewind_only_limit
                                );
                                slog(&session_log, |l| l.warn(&message));
                                record_external_round_inline(
                                    &session_log,
                                    persist_model_responses_inline,
                                    round,
                                    stats.turns,
                                );
                                bus.send(AppEvent::RoundComplete {
                                    session_id: live_session_id.clone(),
                                    round,
                                    turns_in_round: stats.turns,
                                    native_message_count: None,
                                });
                                bus.send(AppEvent::LoopError(message));
                                stats.terminal_outcome = Some(
                                    "managed Codex terminated under context pressure".to_string(),
                                );
                                break;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            slog(&session_log, |l| {
                                l.debug(&format!(
                                    "Could not re-read Codex context pressure after managed termination: {}",
                                    e
                                ))
                            });
                        }
                    }
                }
                slog(&session_log, |l| {
                    l.info(&format!(
                        "External agent terminated: {} (exit code: {:?})",
                        reason, exit_code
                    ));
                });
                bus.send(AppEvent::TaskComplete {
                    session_id: live_session_id.clone(),
                    reason: reason.clone(),
                    summary: stats.last_response.clone(),
                });
                stats.terminal_outcome = Some(reason);
                break;
            }
            DrainOutcome::ChannelClosed => {
                slog(&session_log, |l| {
                    l.info("External agent event channel closed")
                });
                stats.terminal_outcome = Some("external agent event channel closed".to_string());
                break;
            }
        }
    }

    if let Err(e) = agent.shutdown().await {
        slog(&session_log, |l| {
            l.warn(&format!("Agent shutdown error: {}", e))
        });
    }

    Ok(stats)
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
