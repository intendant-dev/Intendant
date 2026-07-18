use crate::agent::{truncate_utf8_by_bytes, Agent};
use crate::error::AgentError;
use crate::models::AgentInput;
use std::io::{self, Read, Write};

mod agent;
mod build_info;
mod error;
mod models;
mod utils;
#[cfg(windows)]
mod win_sandbox;

/// Maximum bytes to read from stdin (64 MB).
const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;
const JSON_PARSE_INPUT_DIAGNOSTIC_BYTES: usize = 2048;

#[cfg(target_os = "linux")]
fn apply_sandbox_from_env() -> Result<(), AgentError> {
    use landlock::{AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI};
    use std::path::PathBuf;

    let paths = std::env::var("INTENDANT_SANDBOX_WRITE_PATHS").unwrap_or_default();
    if paths.trim().is_empty() {
        return Ok(());
    }

    let write_paths: Vec<PathBuf> = std::env::split_paths(&paths)
        .filter(|p| !p.as_os_str().is_empty())
        .collect();

    if write_paths.is_empty() {
        return Ok(());
    }

    let abi = ABI::V5;
    let read_access = AccessFs::from_read(abi);
    let write_access = AccessFs::from_read(abi) | AccessFs::from_write(abi);

    let mut ruleset_created = Ruleset::default()
        .handle_access(write_access)
        .map_err(|e| AgentError::Process(format!("Landlock ruleset creation failed: {}", e)))?
        .create()
        .map_err(|e| AgentError::Process(format!("Landlock ruleset create failed: {}", e)))?;

    // Reads are granted on `/` wholesale, and Landlock cannot subtract
    // from a grant — so project and config `.env` files (where provider
    // keys live) remain readable to sandboxed commands on Linux, unlike
    // the macOS Seatbelt deny clause. Moving keys out of agent-readable
    // files (the credential-custody migration) is the tracked fix; do not
    // mistake this write sandbox for a read boundary.
    if let Ok(root_fd) = PathFd::new("/") {
        ruleset_created = ruleset_created
            .add_rule(PathBeneath::new(root_fd, read_access))
            .map_err(|e| AgentError::Process(format!("Landlock add read rule failed: {}", e)))?;
    }

    // /dev is always write-granted: every Unix process assumes a writable
    // /dev/null and the runtime allocates PTYs (/dev/ptmx, /dev/pts) for
    // command execution — Landlock checks device-file opens like any
    // other, so without this grant even `echo > /dev/null` fails. DAC
    // still applies; this mirrors the macOS Seatbelt profile's
    // unconditional `(allow file-write* (subpath "/dev"))`.
    if let Ok(dev_fd) = PathFd::new("/dev") {
        ruleset_created = ruleset_created
            .add_rule(PathBeneath::new(dev_fd, write_access))
            .map_err(|e| AgentError::Process(format!("Landlock add /dev rule failed: {}", e)))?;
    }

    for path in write_paths {
        if !path.exists() {
            continue;
        }
        if let Ok(fd) = PathFd::new(&path) {
            ruleset_created = ruleset_created
                .add_rule(PathBeneath::new(fd, write_access))
                .map_err(|e| {
                    AgentError::Process(format!("Landlock add write rule failed: {}", e))
                })?;
        }
    }

    let status = ruleset_created
        .restrict_self()
        .map_err(|e| AgentError::Process(format!("Landlock restrict_self failed: {}", e)))?;
    if status.ruleset == landlock::RulesetStatus::NotEnforced {
        // Fail closed: a configured sandbox must never silently degrade to
        // unrestricted execution (scoped shells and an explicitly enabled
        // Windows runtime already refuse in this situation). Linux defaults
        // the sandbox on, so on a Landlock-less kernel this is the error
        // operators see — name the explicit opt-outs rather than degrading
        // silently.
        return Err(AgentError::Process(
            "Filesystem sandbox is enabled (the default) but Landlock is not enforced by this \
             kernel; refusing to run unsandboxed. Pass --no-sandbox or set [sandbox] \
             enabled = false in intendant.toml to explicitly opt out."
                .to_string(),
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn apply_sandbox_from_env() -> Result<(), AgentError> {
    Ok(())
}

/// Write a line to stdout, returning false on broken pipe (caller killed us).
fn write_line(stdout: &mut io::StdoutLock, line: &str) -> bool {
    writeln!(stdout, "{}", line).is_ok() && stdout.flush().is_ok()
}

// Single-threaded runtime: the executor runs its batch strictly
// sequentially (PTY draining runs on dedicated std threads), so the default
// multi-threaded flavor's per-core worker threads bought nothing while
// costing startup time and thread stacks on every tool batch — this binary
// is spawned fresh per batch.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), AgentError> {
    // Version/provenance probe — MUST run before anything touches stdin
    // (the runtime otherwise blocks reading its one-shot JSON input) and
    // before the Windows sandbox re-exec consumes the argv.
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("--version") | Some("-V")
    ) {
        println!("{}", build_info::version_line("intendant-runtime"));
        return Ok(());
    }

    // Initialize logging
    env_logger::init();

    // Windows write-restriction re-exec — MUST run before stdin is read:
    // the restricted child inherits and consumes the still-unread stdin
    // pipe, while this parent only waits and proxies the exit code. Linux
    // restricts in-process below (Landlock needs no re-exec); macOS is
    // wrapped externally in sandbox-exec by the caller. Fail closed.
    #[cfg(windows)]
    match win_sandbox::reexec_write_restricted_if_configured() {
        Ok(None) => {}
        Ok(Some(code)) => std::process::exit(code),
        Err(e) => {
            return Err(AgentError::Process(format!(
                "Windows write sandbox failed (refusing to run unconfined): {e}"
            )));
        }
    }

    // Read entire JSON input (bounded)
    let mut buffer = String::new();
    io::stdin()
        .take(MAX_INPUT_BYTES)
        .read_to_string(&mut buffer)?;

    // Parse single JSON input
    let input: AgentInput = match serde_json::from_str(&buffer) {
        Ok(input) => input,
        Err(e) => {
            eprintln!("JSON parse error: {}", e);
            let preview = truncate_utf8_by_bytes(&buffer, JSON_PARSE_INPUT_DIAGNOSTIC_BYTES);
            let omitted = buffer.len().saturating_sub(preview.len());
            if omitted > 0 {
                eprintln!(
                    "Input was (truncated to {} bytes; omitted {} bytes): {}",
                    preview.len(),
                    omitted,
                    preview
                );
            } else {
                eprintln!("Input was: {}", preview);
            }
            return Err(AgentError::Json(e));
        }
    };

    // Apply filesystem sandbox before running commands.
    apply_sandbox_from_env()?;

    // Create agent instance
    let agent = Agent::new()?.with_human_response_token(input.human_response_token.clone());

    // Process commands sequentially, streaming each JSONL result line as its
    // command completes — the caller consumes partial output when the
    // runtime dies early, so a hard-timeout kill no longer discards the
    // results of commands that already finished. write_line flushes per
    // line and returns false on broken pipe (the caller is gone), which
    // stops the batch gracefully.
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    agent
        .process_input(input, |line| write_line(&mut stdout, line))
        .await?;

    Ok(())
}
