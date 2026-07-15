use crate::error::CallerError;
use crate::sandbox::SandboxConfig;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

/// Maximum bytes to read from agent stdout/stderr (64 MB).
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// Env var the sandboxed runtime consults to decide whether display 0 (the
/// user's real session) is a permitted capture/input target (`src/agent.rs`,
/// `docs/src/runtime-protocol.md`). The controller never sets this on its own
/// process — the autonomy guard (`AutonomyState::user_display_granted`) is
/// the single source of truth, and the flag is derived onto the child's
/// environment here, at the runtime spawn boundary.
const USER_DISPLAY_GRANTED_ENV: &str = "INTENDANT_USER_DISPLAY_GRANTED";

/// Derive the user-display grant onto a runtime child's environment.
/// Absence of the var means "not granted" on the runtime side, so `false`
/// sets nothing.
fn apply_user_display_grant_env(cmd: &mut Command, user_display_granted: bool) {
    if user_display_granted {
        cmd.env(USER_DISPLAY_GRANTED_ENV, "1");
    }
}

/// Provider-credential env names scrubbed from the runtime child beyond the
/// authoritative `provider::PROVIDER_KEY_ENV_VARS` list: adjacent
/// conventional spellings of the same secrets that a user `.env` (loaded
/// into the controller's process env at startup) may carry, plus the bare
/// vendor names `exec_as_agent` has always scrubbed from its shells.
const EXTRA_PROVIDER_CREDENTIAL_ENV_VARS: &[&str] = &[
    "GOOGLE_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI",
    "ANTHROPIC",
    "GEMINI",
];

/// True when `name` names a provider/model-API credential that must not
/// reach the runtime. `INTENDANT_*` names are the controller→runtime
/// control channel (the mock-provider e2e rig rides `PROVIDER` +
/// `INTENDANT_MOCK_*` into children) and are never treated as credentials.
fn is_provider_credential_env(name: &str) -> bool {
    if name.starts_with("INTENDANT_") {
        return false;
    }
    crate::provider::PROVIDER_KEY_ENV_VARS.contains(&name)
        || EXTRA_PROVIDER_CREDENTIAL_ENV_VARS.contains(&name)
        || name.ends_with("_API_KEY")
        || name.ends_with("_API_TOKEN")
}

/// Scrub provider credentials from the runtime child's environment.
///
/// The controller loads `.env` provider keys into its own process env at
/// startup, and a spawned child inherits that env wholesale — without this
/// scrub the sandboxed runtime (and every exec/PTY shell it spawns) holds
/// the model API keys, violating the founding runtime/controller boundary
/// ("the runtime never holds API keys"): a model-invoked
/// `echo $ANTHROPIC_API_KEY` in a PTY shell would exfiltrate the key into
/// the conversation. This spawn boundary is the single enforcement point;
/// `exec_as_agent`'s per-shell env_removes remain as defense in depth.
/// `inherited_names` is the parent-process env view (injected by the caller
/// so tests stay hermetic).
fn scrub_provider_credential_env<'a>(
    cmd: &mut Command,
    inherited_names: impl IntoIterator<Item = &'a str>,
) {
    // The canonical names are removed unconditionally — even when absent
    // from the inherited env view — so an explicit `.env()` set can never
    // reintroduce them.
    for name in crate::provider::PROVIDER_KEY_ENV_VARS
        .iter()
        .chain(EXTRA_PROVIDER_CREDENTIAL_ENV_VARS.iter())
    {
        cmd.env_remove(name);
    }
    for name in inherited_names {
        if is_provider_credential_env(name) {
            cmd.env_remove(name);
        }
    }
}

pub struct AgentOutput {
    pub stdout: String,
    pub stderr: String,
}

fn has_ask_human(json_input: &str) -> bool {
    let parsed: serde_json::Value = match serde_json::from_str(json_input) {
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

fn has_parseable_runtime_output(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout).lines().any(|line| {
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            return false;
        };
        matches!(
            parsed.get("type").and_then(|value| value.as_str()),
            Some("status" | "result")
        ) && parsed
            .get("nonce")
            .and_then(|value| value.as_u64())
            .is_some()
    })
}

fn output_with_exit_status(
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
    status: ExitStatus,
) -> Result<AgentOutput, CallerError> {
    let stdout = String::from_utf8_lossy(&stdout_buf).to_string();
    let mut stderr = String::from_utf8_lossy(&stderr_buf).to_string();
    if !status.success() {
        if !has_parseable_runtime_output(&stdout_buf) {
            let stderr_tail = stderr.trim();
            let detail = if stderr_tail.is_empty() {
                String::new()
            } else {
                format!("; stderr: {stderr_tail}")
            };
            return Err(CallerError::Agent(format!(
                "sandboxed runtime exited with {status} before producing parseable output{detail}"
            )));
        }
        if !stderr.ends_with('\n') && !stderr.is_empty() {
            stderr.push('\n');
        }
        stderr.push_str(&format!(
            "sandboxed runtime exited with {status} after producing output"
        ));
    }
    Ok(AgentOutput { stdout, stderr })
}

/// Spawn the sandboxed runtime for one command batch.
///
/// `user_display_granted` is the autonomy guard's grant state, read by the
/// caller at spawn time — the runtime child observes it as
/// `INTENDANT_USER_DISPLAY_GRANTED` on its environment.
pub async fn run_agent(
    json_input: &str,
    log_dir: &std::path::Path,
    workdir: &std::path::Path,
    user_display_granted: bool,
) -> Result<AgentOutput, CallerError> {
    // Linux enforces this via Landlock inside the runtime; macOS wraps the
    // runtime in sandbox-exec; Windows re-execs inside the runtime under a
    // write-restricted token (win_sandbox.rs) — see run_agent_inner.
    if let Ok(raw_paths) = std::env::var("INTENDANT_SANDBOX_WRITE_PATHS") {
        let write_paths: Vec<PathBuf> = std::env::split_paths(&raw_paths)
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        if !write_paths.is_empty() {
            let sandbox = SandboxConfig {
                read_paths: vec![PathBuf::from("/")],
                write_paths,
                enabled: true,
            };
            return run_agent_inner(
                json_input,
                log_dir,
                Some(workdir),
                Some(&sandbox),
                user_display_granted,
            )
            .await;
        }
    }
    run_agent_inner(
        json_input,
        log_dir,
        Some(workdir),
        None,
        user_display_granted,
    )
    .await
}

/// Run the agent with optional Landlock sandbox configuration.
#[allow(dead_code)]
pub async fn run_agent_sandboxed(
    json_input: &str,
    log_dir: &std::path::Path,
    sandbox: &crate::sandbox::SandboxConfig,
    user_display_granted: bool,
) -> Result<AgentOutput, CallerError> {
    run_agent_inner(
        json_input,
        log_dir,
        None,
        Some(sandbox),
        user_display_granted,
    )
    .await
}

async fn run_agent_inner(
    json_input: &str,
    log_dir: &std::path::Path,
    workdir: Option<&std::path::Path>,
    sandbox: Option<&crate::sandbox::SandboxConfig>,
    user_display_granted: bool,
) -> Result<AgentOutput, CallerError> {
    let agent_path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("intendant-runtime")))
        .unwrap_or_else(|| std::path::PathBuf::from("./target/debug/intendant-runtime"));

    // macOS parity with the Linux Landlock posture: the runtime child is
    // wrapped in sandbox-exec with a write-restricting Seatbelt profile
    // (reads stay open, writes confined to the configured paths). With no
    // write sandbox configured the child is still wrapped in the
    // sensitive-only profile — user-secret directories (~/.ssh, ~/.gnupg)
    // are denied at the OS level, closing the executeCommand bypass of the
    // runtime's validate_path denylist (which only sees structured tool
    // arguments). Linux applies its write restriction inside the runtime
    // via the env var below; Landlock cannot subtract read access, so the
    // secret-directory guard has no Linux equivalent. A profile that fails
    // to generate fails the spawn rather than silently running unconfined.
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let profile = match sandbox.filter(|sandbox| sandbox.enabled) {
            Some(sandbox) => sandbox.seatbelt_write_only_profile(),
            None => crate::sandbox::seatbelt_sensitive_only_profile(),
        }
        .map_err(|e| CallerError::Agent(format!("sandbox profile: {e}")))?;
        let mut cmd = Command::new("/usr/bin/sandbox-exec");
        cmd.arg("-p").arg(profile).arg(&agent_path);
        cmd
    };
    #[cfg(not(target_os = "macos"))]
    let mut cmd = Command::new(&agent_path);

    cmd.env("INTENDANT_LOG_DIR", log_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Run the runtime from the session's project root so relative paths in
    // commands resolve where the conversation says they do ("Working
    // directory: <project root>"). Without this the runtime inherited the
    // controller's cwd, which diverges for any session whose project_root
    // differs from the daemon's launch directory — sub-agent children and
    // dashboard sessions targeting other projects. (The retired subprocess
    // pipeline got this via `cd <dir> &&` in its spawn shell string.)
    if let Some(workdir) = workdir.filter(|dir| dir.is_dir()) {
        cmd.current_dir(workdir);
    }

    // If sandbox config is provided, serialize it as an env var. The
    // runtime applies the restriction itself at startup — Landlock on
    // Linux, a write-restricted token re-exec on Windows.
    #[cfg(any(target_os = "linux", windows))]
    if let Some(sandbox) = sandbox {
        if sandbox.enabled {
            if let Ok(joined) = std::env::join_paths(&sandbox.write_paths) {
                cmd.env("INTENDANT_SANDBOX_WRITE_PATHS", joined);
            }
        }
    }

    // Derive the user-display grant from the autonomy guard (passed in by
    // the caller) onto the child env. This spawn boundary is the only place
    // the grant becomes an environment variable — the controller's own
    // process env is never mutated with it.
    apply_user_display_grant_env(&mut cmd, user_display_granted);

    // Also preserve the original user display for UserSession resolution
    if std::env::var("INTENDANT_USER_DISPLAY").is_ok() {
        if let Ok(val) = std::env::var("INTENDANT_USER_DISPLAY") {
            cmd.env("INTENDANT_USER_DISPLAY", val);
        }
    }

    #[cfg(target_os = "linux")]
    crate::linux_display_env::apply_to_tokio_command(&mut cmd);

    // The runtime never holds API keys (see the scrub's doc): strip provider
    // credentials from the child env as the last step before spawn.
    let inherited_env_names: Vec<String> = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .collect();
    scrub_provider_credential_env(&mut cmd, inherited_env_names.iter().map(String::as_str));

    let mut child = cmd.spawn().map_err(|e| {
        CallerError::Agent(format!("Failed to spawn agent at {:?}: {}", agent_path, e))
    })?;

    // Write JSON to stdin and close it
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(json_input.as_bytes()).await?;
        // stdin dropped here, closing the pipe
    }

    // Hard timeout: 120s default, no timeout for askHuman (polls indefinitely)
    let has_human = has_ask_human(json_input);
    let hard_timeout_secs: u64 = if has_human { u64::MAX / 2 } else { 120 };
    let hard_timeout = Duration::from_secs(hard_timeout_secs);

    // Read stdout and stderr (bounded), then wait for exit, all under a single hard timeout
    let result = timeout(hard_timeout, async {
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();

        let read_stdout = async {
            let mut buf = Vec::with_capacity(8192);
            if let Some(ref mut out) = stdout {
                out.take(MAX_OUTPUT_BYTES as u64)
                    .read_to_end(&mut buf)
                    .await?;
            }
            Ok::<_, std::io::Error>(buf)
        };
        let read_stderr = async {
            let mut buf = Vec::with_capacity(8192);
            if let Some(ref mut err) = stderr {
                err.take(MAX_OUTPUT_BYTES as u64)
                    .read_to_end(&mut buf)
                    .await?;
            }
            Ok::<_, std::io::Error>(buf)
        };

        let (stdout_buf, stderr_buf, status) = tokio::join!(read_stdout, read_stderr, child.wait());
        Ok::<_, CallerError>((stdout_buf?, stderr_buf?, status?))
    })
    .await;

    match result {
        Ok(Ok((stdout_buf, stderr_buf, status))) => {
            output_with_exit_status(stdout_buf, stderr_buf, status)
        }
        Ok(Err(err)) => Err(err),
        Err(_) => {
            let _ = child.kill().await;
            Err(CallerError::Agent(format!(
                "Agent timed out after {}s",
                hard_timeout_secs
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_ask_human_detects_function() {
        let json = r#"{"commands":[{"function":"askHuman","nonce":1,"question":"Which DB?"}]}"#;
        assert!(has_ask_human(json));
    }

    #[test]
    fn has_ask_human_false_for_other() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"}]}"#;
        assert!(!has_ask_human(json));
    }

    #[test]
    fn has_ask_human_mixed_commands() {
        let json = r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"ls"},{"function":"askHuman","nonce":2,"question":"ok?"}]}"#;
        assert!(has_ask_human(json));
    }

    #[test]
    fn has_ask_human_false_for_text_only() {
        let json =
            r#"{"commands":[{"function":"execAsAgent","nonce":1,"command":"echo \"askHuman\""}]}"#;
        assert!(!has_ask_human(json));
    }

    #[test]
    fn parseable_runtime_output_requires_nonce_result_or_status() {
        assert!(has_parseable_runtime_output(
            br#"{"type":"result","nonce":7,"data":"ok"}"#
        ));
        assert!(has_parseable_runtime_output(
            br#"noise
{"type":"status","nonce":7,"status":"E","exit_code":1}"#
        ));
        assert!(!has_parseable_runtime_output(
            br#"{"type":"result","data":"missing nonce"}"#
        ));
        assert!(!has_parseable_runtime_output(b"panic before json"));
    }

    #[test]
    fn provider_credential_env_predicate_covers_keys_not_control_vars() {
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "MISTRAL_API_KEY",
            "SOME_SERVICE_API_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
        ] {
            assert!(is_provider_credential_env(name), "{name} must be scrubbed");
        }
        for name in [
            "PROVIDER",
            "PATH",
            "HOME",
            "DISPLAY",
            "INTENDANT_MOCK_SCRIPT",
            "INTENDANT_MOCK_DISPLAY",
            "INTENDANT_LOG_DIR",
            "INTENDANT_FAKE_API_KEY", // the INTENDANT_* namespace is never scrubbed
            "OPENAI_BASE_URL",
        ] {
            assert!(!is_provider_credential_env(name), "{name} must survive");
        }
    }

    /// The founding invariant: the runtime child's env never carries
    /// provider API keys, while the mock-provider e2e control vars and the
    /// runtime's own INTENDANT_* channel survive. Hermetic — the
    /// inherited-env view is injected; no real keys, no process env.
    #[test]
    fn runtime_child_env_scrubs_provider_credentials() {
        use std::ffi::{OsStr, OsString};

        let mut cmd = Command::new("true");
        cmd.env("INTENDANT_LOG_DIR", "/tmp/logs");
        scrub_provider_credential_env(
            &mut cmd,
            [
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "GEMINI_API_KEY",
                "MISTRAL_API_KEY",
                "CUSTOM_API_TOKEN",
                "PATH",
                "HOME",
                "PROVIDER",
                "INTENDANT_MOCK_SCRIPT",
            ],
        );
        let envs: std::collections::HashMap<OsString, Option<OsString>> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
            .collect();

        // Removed vars appear as explicit (name, None) child-env entries.
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "MISTRAL_API_KEY",
            "CUSTOM_API_TOKEN",
        ] {
            assert_eq!(
                envs.get(OsStr::new(name)),
                Some(&None),
                "{name} must be removed from the child env"
            );
        }
        // Preserved vars have no explicit entry at all: they inherit.
        for name in ["PATH", "HOME", "PROVIDER", "INTENDANT_MOCK_SCRIPT"] {
            assert!(
                !envs.contains_key(OsStr::new(name)),
                "{name} must inherit untouched (no explicit entry)"
            );
        }
        assert_eq!(
            envs.get(OsStr::new("INTENDANT_LOG_DIR")),
            Some(&Some(OsString::from("/tmp/logs"))),
            "runtime control vars set at the spawn boundary must survive the scrub"
        );
    }

    /// The spawn boundary is the only place the user-display grant becomes
    /// an environment variable: granted derives the exact var name the
    /// runtime reads (`src/agent.rs`), ungranted leaves the child env
    /// untouched (absence = denied on the runtime side). The controller's
    /// own process env plays no part.
    #[test]
    fn user_display_grant_env_derives_from_guard_state_at_spawn() {
        let mut cmd = Command::new("true");
        apply_user_display_grant_env(&mut cmd, true);
        let env: Vec<_> = cmd.as_std().get_envs().collect();
        assert!(
            env.contains(&(
                std::ffi::OsStr::new("INTENDANT_USER_DISPLAY_GRANTED"),
                Some(std::ffi::OsStr::new("1"))
            )),
            "granted state must set the runtime's grant var on the child: {env:?}"
        );

        let mut cmd = Command::new("true");
        apply_user_display_grant_env(&mut cmd, false);
        assert!(
            cmd.as_std()
                .get_envs()
                .all(|(k, _)| k != std::ffi::OsStr::new("INTENDANT_USER_DISPLAY_GRANTED")),
            "ungranted state must not set the grant var on the child"
        );
    }
}
