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

pub async fn run_agent(
    json_input: &str,
    log_dir: &std::path::Path,
    workdir: &std::path::Path,
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
            return run_agent_inner(json_input, log_dir, Some(workdir), Some(&sandbox)).await;
        }
    }
    run_agent_inner(json_input, log_dir, Some(workdir), None).await
}

/// Run the agent with optional Landlock sandbox configuration.
#[allow(dead_code)]
pub async fn run_agent_sandboxed(
    json_input: &str,
    log_dir: &std::path::Path,
    sandbox: &crate::sandbox::SandboxConfig,
) -> Result<AgentOutput, CallerError> {
    run_agent_inner(json_input, log_dir, None, Some(sandbox)).await
}

async fn run_agent_inner(
    json_input: &str,
    log_dir: &std::path::Path,
    workdir: Option<&std::path::Path>,
    sandbox: Option<&crate::sandbox::SandboxConfig>,
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

    // Pass through user display grant if set by the caller
    if std::env::var("INTENDANT_USER_DISPLAY_GRANTED").is_ok() {
        cmd.env("INTENDANT_USER_DISPLAY_GRANTED", "1");
    }

    // Also preserve the original user display for UserSession resolution
    if std::env::var("INTENDANT_USER_DISPLAY").is_ok() {
        if let Ok(val) = std::env::var("INTENDANT_USER_DISPLAY") {
            cmd.env("INTENDANT_USER_DISPLAY", val);
        }
    }

    #[cfg(target_os = "linux")]
    crate::linux_display_env::apply_to_tokio_command(&mut cmd);

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
}
