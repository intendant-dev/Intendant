mod access;
mod agent_runner;
mod app_state_pricing;
mod approval;
mod atspi_read;
mod audio_routing;
pub(crate) use intendant_core::autonomy;
#[cfg(target_os = "macos")]
mod ax;
mod browser_workspace;
mod computer_use;
mod connect_rendezvous;
mod context_rewind;
mod control;
mod control_plane;
pub(crate) use intendant_core::conversation;
mod credential_audit;
mod credential_egress;
mod credential_leases;
mod ctl;
mod daemon_identity;
mod daemon_log_tee;
mod dashboard_control;
mod debug;
mod diagnostics;
pub(crate) use intendant_display as display;
pub(crate) use intendant_core::error;
mod event;
mod external_agent;
mod external_wrapper_index;
mod file_watcher;
mod fission_ledger;
mod fission_lifecycle;
pub(crate) use intendant_core::frames;
mod frontend;
mod gateway_routes;
pub(crate) use intendant_core::knowledge;
mod lineage_ledger;
mod linux_display_env;
mod live_audio;
mod live_audio_types;
mod mcp;
mod mcp_client;
mod peer;
mod peer_file_transfer;
pub(crate) use intendant_platform::platform;
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
mod session_vitals;
mod usage_rail;
mod setup;
pub(crate) use intendant_core::skills;
mod sub_agent;
mod task_dispatch;
mod terminal;
#[cfg(test)]
mod test_support;
mod tool_batch;
mod tools;
mod transcription;
mod transfer_store;
mod types;
mod upload_store;
mod virtual_display;
pub(crate) use intendant_platform::vision;
mod web_gateway;
mod web_tls;
#[cfg(windows)]
#[path = "../../win_sandbox.rs"]
mod win_sandbox;
mod windows_uia;
mod worktree;
mod worktree_inventory;
#[cfg(target_os = "linux")]
pub(crate) use intendant_display::x11_input;

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
mod external_supervision;
pub(crate) use external_supervision::*;
mod display_glue;
pub(crate) use display_glue::*;

use autonomy::{AutonomyLevel, AutonomyState, SharedAutonomy};
use conversation::Conversation;
use error::CallerError;
use event::{AppEvent, ControlMsg, EventBus};
use project::Project;
use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{self, BufRead, Write};
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

fn event_targets_session(target: &Option<String>, session_id: &Option<String>) -> bool {
    match target {
        Some(target) => session_id.as_deref() == Some(target.as_str()),
        None => true,
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

/// CLI flags parsed from command-line arguments.
struct CliFlags {
    task: Option<String>,
    /// --task-file <PATH>: Read the initial task from a file instead of argv.
    task_file: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    verbose: bool,
    /// --no-tui: accepted for compatibility (the terminal TUI was removed);
    /// has no effect. Headless output is --no-web; the dashboard is the UI.
    #[allow(dead_code)]
    no_tui: bool,
    mcp: bool,
    autonomy: AutonomyLevel,
    log_file: Option<String>,
    /// --continue / -c: resume the most recent session for this project.
    continue_last: bool,
    /// --resume / -r [id]: resume a specific session by ID or path.
    resume_id: Option<String>,
    control_socket: bool,
    /// --json: Emit JSONL events to stdout (headless stdio; implies --no-web).
    json_output: bool,
    /// --sandbox: Enable Landlock filesystem sandboxing for the runtime.
    #[allow(dead_code)]
    sandbox: bool,
    /// --direct: Force single-agent mode (skip orchestrator/sub-agent delegation).
    /// Does NOT disable the UI — use --no-web for headless output.
    direct: bool,
    /// --no-presence: Disable the presence layer (direct agent interaction).
    no_presence: bool,
    /// --web [PORT]: Serve the web dashboard (on by default).
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
    println!("    --no-tui              Deprecated no-op (the terminal TUI was removed)");
    println!("    --mcp                 Run as MCP server on stdio");
    println!("    --verbose, -v         Enable verbose output");
    println!("    --control-socket      Enable Unix control socket");
    println!("    --json                Emit JSONL events to stdout (headless stdio)");
    println!("    --sandbox             Enable Landlock filesystem sandboxing for the runtime");
    println!("    --direct              Force single-agent mode (skip orchestrator/sub-agent delegation)");
    println!("    --no-presence         Disable the presence layer (direct agent interaction)");
    println!("    --web [PORT]          Web dashboard (default: on, port 8765; idle start runs the daemon)");
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
    parse_cli_flags_from(env::args().skip(1).collect())
}

/// Testable core of [`parse_cli_flags`]: `args` is argv minus the binary
/// name.
fn parse_cli_flags_from(args: Vec<String>) -> Result<CliFlags, CallerError> {
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
                // Deprecated no-op, accepted for compatibility (the terminal
                // TUI was removed; the dashboard is the UI, --no-web is
                // headless).
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
                // Optional-value flag: `--resume` without an id acts like
                // `--continue`, so a dash-leading next token is read as the
                // next flag, not as an id. That is only safe because resume
                // ids are UUIDs (SessionLog::resolve_path) or external-CLI
                // session ids (also UUIDs) — both hex-leading. If a resume
                // id mint ever moves to a dashable alphabet (base64url), the
                // id must get a fixed alphanumeric prefix (see 8c9c0d96).
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
            "--" => {
                // Standard end-of-flags separator: everything after is task
                // text, however it starts — the escape hatch for prose (or a
                // pasted token) that would otherwise read as a flag.
                task_parts.extend(args[i + 1..].iter().cloned());
                break;
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
    match fission_lifecycle::rehydrate_from_logs(&platform::intendant_home().join("logs")) {
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
        // Answer resolves question prompts; it grants nothing.
        event::ApprovalResponse::Skip
        | event::ApprovalResponse::Deny
        | event::ApprovalResponse::Answer { .. } => return,
    }
    if cat == autonomy::ActionCategory::DisplayControl && !state.user_display_granted {
        state.user_display_granted = true;
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
    fn idle_web_defaults_to_daemon() {
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

        let task = resolve_initial_task_for_startup(&flags, false).unwrap();
        assert_eq!(task.as_deref(), Some("serve this task over mcp"));
    }

    #[test]
    fn web_task_file_is_initial_task() {
        let dir = tempfile::tempdir().unwrap();
        let task_path = dir.path().join("task.txt");
        std::fs::write(&task_path, "resume managed harness\n").unwrap();

        let mut flags = cli_flags_for_tests();
        flags.task_file = Some(task_path.to_string_lossy().into_owned());
        flags.web = true;
        flags.no_presence = true;
        flags.agent_backend = Some(external_agent::AgentBackend::Codex);
        flags.resume_id = Some("6036429e-54f9-4f93-b74d-04c060c79054".to_string());

        let web_daemon_requested = should_start_idle_web_daemon(true, &flags);
        assert!(!web_daemon_requested);
        let task = resolve_initial_task_for_startup(&flags, web_daemon_requested).unwrap();
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

    fn cli(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_cli_flags_double_dash_ends_flags() {
        // `--` is the standard escape hatch: everything after it is task
        // text even when it would otherwise parse as a flag.
        let flags = parse_cli_flags_from(cli(&["--direct", "--", "--not-a-flag", "task"]))
            .expect("everything after -- is task text");
        assert!(flags.direct);
        assert_eq!(flags.task.as_deref(), Some("--not-a-flag task"));
    }

    #[test]
    fn parse_cli_flags_unknown_flag_still_errors() {
        // (match instead of unwrap_err: CliFlags deliberately has no Debug.)
        for argv in [cli(&["--nope"]), cli(&["-x", "task"])] {
            match parse_cli_flags_from(argv) {
                Err(err) => assert!(err.to_string().contains("Unknown CLI flag")),
                Ok(_) => panic!("unknown flag must error"),
            }
        }
    }

    #[test]
    fn parse_cli_flags_resume_takes_id_but_yields_to_flags() {
        // A hex-leading id (UUIDs — the only resume-id mints) is captured…
        let flags =
            parse_cli_flags_from(cli(&["--resume", "0f9b2c1e-aaaa-bbbb-cccc-121212121212"]))
                .unwrap();
        assert_eq!(
            flags.resume_id.as_deref(),
            Some("0f9b2c1e-aaaa-bbbb-cccc-121212121212")
        );
        assert!(!flags.continue_last);
        // …while a dash-leading next token keeps the documented
        // optional-value fallback: --resume degrades to --continue.
        let flags = parse_cli_flags_from(cli(&["--resume", "--direct"])).unwrap();
        assert!(flags.continue_last);
        assert!(flags.direct);
        assert!(flags.resume_id.is_none());
    }

    #[test]
    fn parse_cli_flags_value_flags_accept_dash_leading_values() {
        // Value positions must never reject a token for its leading dash:
        // --owner takes a base64url fingerprint, 1 in 64 of which starts
        // with '-'.
        let fingerprint = "-vyeJaE3hyqm4J45K5j_sVTXAAAABBBBCCCCDDDDEEE";
        assert_eq!(fingerprint.len(), 43);
        let flags = parse_cli_flags_from(cli(&["--owner", fingerprint])).unwrap();
        assert_eq!(flags.owner.as_deref(), Some(fingerprint));
    }

    #[test]
    fn parse_cli_flags_empty() {
        // Struct defaults (parser behavior is covered above).
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

/// Spawn a listener that reacts to display grant/revoke events.
/// On grant: create a DisplaySession (Wayland) and emit DisplayReady.
/// On revoke: stop the session and remove it from the registry.
///
/// Also the owner of dashboard-created virtual displays: it consumes
/// `ControlMsg::CreateVirtualDisplay` off the bus (this task is the one
/// place with the session/frame registries the create needs), and holds
/// their `XvfbGuard`s as task-local state — created displays die with the
/// daemon, on tile close (revoke of their id), or on capture loss.
pub fn spawn_user_display_listener(
    bus: EventBus,
    session_registry: display::SharedSessionRegistry,
    frame_registry: Option<std::sync::Arc<tokio::sync::RwLock<frames::FrameRegistry>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = bus.subscribe();
        let mut virtual_display_guards = virtual_display::VirtualDisplayGuards::new();
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
                Ok(AppEvent::ControlCommand(ControlMsg::CreateVirtualDisplay {
                    width,
                    height,
                })) => {
                    virtual_display::create_virtual_display(
                        &bus,
                        &session_registry,
                        frame_registry.clone(),
                        &mut virtual_display_guards,
                        width,
                        height,
                    )
                    .await;
                }
                Ok(AppEvent::UserDisplayRevoked { display_id, .. }) => {
                    deactivate_user_display(&session_registry, display_id).await;
                    // Closing the tile of a dashboard-created display IS its
                    // destroy: nothing else owns it, and leaving the Xvfb
                    // running would leak displays with no way to kill them.
                    virtual_display::reap_virtual_display(
                        &mut virtual_display_guards,
                        display_id,
                        "tile closed",
                    );
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
                    virtual_display::reap_virtual_display(
                        &mut virtual_display_guards,
                        display_id,
                        "capture lost",
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                _ => {}
            }
        }
    })
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

fn configure_sandbox_env(
    flags: &CliFlags,
    project: &Project,
    log_dir: &std::path::Path,
    // `None` = projectless daemon: the write scope is scratch + logs +
    // ~/.intendant + explicit absolute grants only — the launch cwd never
    // becomes writable by accident. Every other path passes the project root.
    project_write_scope: Option<&std::path::Path>,
) {
    let enabled = flags.sandbox || project.config.sandbox.enabled;
    if !enabled {
        env::remove_var("INTENDANT_SANDBOX_WRITE_PATHS");
        return;
    }

    let mut sandbox_cfg = match project_write_scope {
        Some(root) => sandbox::SandboxConfig::default_for_project(root, log_dir),
        None => sandbox::SandboxConfig::projectless(log_dir),
    };
    for p in &project.config.sandbox.extra_write_paths {
        let extra = if std::path::Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else if let Some(root) = project_write_scope {
            root.join(p)
        } else {
            // No project to anchor a relative grant to; resolving against
            // the launch cwd would silently widen the projectless scope.
            // (Unreachable in practice — projectless implies no
            // intendant.toml, so this list is empty — kept fail-closed.)
            eprintln!(
                "[sandbox] relative extra write path {p} ignored: the daemon has no project root to resolve it against"
            );
            continue;
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
    // — recorded so credential errors can name the concrete paths searched
    // (under a daemon, "project root" is the launch dir, not the project a
    // session was created with; see provider::EnvSearchReport).
    let cwd_env = dotenvy::dotenv().ok();
    let mut project = Project::detect()?;
    // A root without a project marker (.git / intendant.toml) is just the
    // launch cwd — its .env, if any, was already covered by the cwd walk
    // above. Don't load (or report) it a second time as a "project root"
    // layer: under a daemon that label misled credential errors.
    let project_has_marker = project::root_has_project_marker(&project.root);
    let project_env = project_has_marker.then(|| {
        let path = project.root.join(".env");
        let loaded = dotenvy::from_path(&path).is_ok();
        (path, loaded)
    });
    let global_env = dirs::config_dir().map(|config_dir| {
        let path = config_dir.join("intendant").join(".env");
        let loaded = dotenvy::from_path(&path).is_ok();
        (path, loaded)
    });
    provider::record_env_search(provider::EnvSearchReport {
        cwd_env,
        project_env,
        global_env,
    });

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
    // Idle web/dashboard startup defaults to the daemon path (the session
    // supervisor owns all launches). Computed here — from flags only, which
    // are not mutated below — because the daemon shape decides resume
    // ownership, the sandbox write scope, and whether a markerless cwd
    // becomes a project at all.
    let web_daemon_requested =
        should_start_idle_web_daemon(!flags.no_web && !flags.mcp && !flags.json_output, &flags);
    // Projectless: a daemon launched from a directory with no project marker
    // (.git / intendant.toml — `project::root_has_project_marker`) runs
    // without a project instead of adopting cwd; an installed app or service
    // otherwise inherits an accident of launch directory as its sandbox
    // scope, watcher root, and default session project. CLI (headless/MCP)
    // invocations keep cwd-as-project — correct for `intendant "task"`
    // inside a repo.
    let projectless_daemon = web_daemon_requested && !project_has_marker;
    let daemon_project_root: Option<PathBuf> =
        (!projectless_daemon).then(|| project.root.clone());
    if projectless_daemon {
        eprintln!(
            "Projectless daemon: {} has no project marker (.git or intendant.toml) — \
             running without a default project; sessions choose their own project directory",
            project.root.display()
        );
    }
    // Create or resume session log.
    //
    // Under the default-web daemon, --continue/--resume are owned by the
    // SUPERVISOR: the daemon starts on a fresh base log and resumes the
    // target session through ResumeSession at startup. (These flags used to
    // be silently swallowed — the daemon adopted the old session's log dir
    // and then idled.)
    let daemon_owns_resume =
        web_daemon_requested && (flags.continue_last || flags.resume_id.is_some());
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
    // alongside session.jsonl and turn files.
    {
        let daemon_log_path = log_dir.join("daemon.log");
        if let Err(e) = daemon_log_tee::install(&daemon_log_path) {
            eprintln!(
                "daemon_log_tee: could not tee stderr/stdout to {}: {}",
                daemon_log_path.display(),
                e
            );
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

    configure_sandbox_env(&flags, &project, &log_dir, daemon_project_root.as_deref());

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
    // both SIGTERM (external shutdown) and SIGINT (Ctrl+C) so the session
    // doesn't linger as `"status": "running"` in ~/.intendant/logs/ forever.
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
                let _ = credential_leases::revoke(None, "daemon shutdown", "local");
                // Clean up control socket
                control::cleanup();
                std::process::exit(130);
            }
        });
    }

    // Write session metadata (project root, task will be filled in later if
    // available). A projectless daemon's base session honestly records no
    // project instead of attributing itself to the launch cwd.
    slog(&session_log, |l| {
        l.write_meta(daemon_project_root.as_deref(), None);
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
    let (web_port, web_listener) = if use_web {
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
        match daemon_project_root.as_deref() {
            Some(root) => l.debug(&format!("Project root: {}", root.display())),
            None => l.debug(&format!(
                "Project root: none (projectless daemon; launched from {})",
                project.root.display()
            )),
        }
        l.debug(&format!("Autonomy: {}", flags.autonomy));
    });

    // web_daemon_requested (computed above, before the session log): idle
    // web/dashboard startup defaults to the daemon path — the session
    // supervisor owns all launches. A task on the command line runs it as
    // the foreground (headless) session under the same gateway.

    // Task resolution: the daemon starts idle; MCP mode allows starting
    // without a task (it honors an explicit --task-file but must not
    // otherwise call get_task_from_flags_or_env() because it would print to
    // stdout and read from stdin, both reserved for JSON-RPC). The
    // foreground (headless) mode requires a task upfront — on a terminal,
    // get_task_from_flags_or_env prompts for one.
    let task = resolve_initial_task_for_startup(&flags, web_daemon_requested)?;

    if let Some(ref t) = task {
        slog(&session_log, |l| l.info(&format!("Task: {}", t)));
    }

    // Build autonomy state from project config + CLI flags
    let autonomy_state = AutonomyState::new(flags.autonomy, project.config.approval.clone());
    let autonomy = autonomy::shared_autonomy(autonomy_state);

    if web_daemon_requested {
        return startup::daemon::run_daemon(
            &flags,
            &project,
            daemon_project_root,
            autonomy,
            session_log,
            log_dir,
            web_port,
            web_bind_ip,
            web_port_for_agent,
            web_listener,
            web_tls_client_cert_required,
            web_tls_acceptor,
            runtime_presence_enabled,
            initial_agent_backend,
            shared_external_agent,
            frame_registry,
            session_registry,
            recording_registry,
            daemon_startup_resume_dir,
            usage_rail::ProviderIdentity::from_provider(provider.as_deref()),
        )
        .await;
    }

    if flags.mcp {
        startup::mcp_mode::run_mcp_mode(
            &flags,
            &project,
            autonomy,
            session_log,
            log_dir,
            task,
            provider,
            use_web,
            web_port,
            web_bind_ip,
            web_port_for_agent,
            web_listener,
            web_tls_client_cert_required,
            web_tls_acceptor,
            runtime_presence_enabled,
            initial_agent_backend,
            shared_external_agent,
            frame_registry,
            session_registry,
            recording_registry,
        )
        .await?;
    } else {
        startup::headless::run_headless_mode(
            &flags,
            project,
            autonomy,
            session_log,
            log_dir,
            task,
            provider,
            use_web,
            web_port,
            web_bind_ip,
            web_port_for_agent,
            web_listener,
            web_tls_client_cert_required,
            web_tls_acceptor,
            runtime_presence_enabled,
            initial_agent_backend,
            shared_external_agent,
            frame_registry,
            session_registry,
            recording_registry,
            startup_external_resume_session,
        )
        .await?;
    }

    Ok(())
}
