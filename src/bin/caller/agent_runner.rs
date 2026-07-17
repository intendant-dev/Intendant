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

/// Read up to `cap` bytes from `reader` into `buf`, then, if the cap was
/// reached, keep reading and DISCARDING the remainder until EOF so the
/// writer never blocks on a full pipe. Returns how many bytes were
/// discarded.
///
/// The cap bounds what we *buffer*, not what we *consume*: a plain capped
/// read (`take(cap).read_to_end(..)`) stops consuming at the cap while the
/// pipe stays open, so a child with more output than the cap blocks forever
/// on the full pipe buffer and `child.wait()` never resolves. The batch
/// hard-timeout eventually reaps that — except for askHuman batches, whose
/// timeout is effectively infinite, where the session hangs permanently.
///
/// Output landing exactly at the cap drains zero bytes and reports no
/// discard.
async fn read_capped_then_drain<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    cap: u64,
    buf: &mut Vec<u8>,
) -> std::io::Result<u64> {
    let mut limited = reader.take(cap);
    let read = limited.read_to_end(buf).await? as u64;
    if read < cap {
        // EOF arrived under the cap: nothing left in the stream to drain.
        return Ok(0);
    }
    tokio::io::copy(&mut limited.into_inner(), &mut tokio::io::sink()).await
}

/// Append an honest truncation note for one over-cap stream to the batch's
/// stderr, so the model knows that stream was cut rather than complete.
/// Streams that fit (`discarded == 0`, including exactly-at-cap output)
/// append nothing.
fn append_over_cap_note(stderr_buf: &mut Vec<u8>, stream: &str, discarded: u64) {
    if discarded == 0 {
        return;
    }
    if !stderr_buf.is_empty() && !stderr_buf.ends_with(b"\n") {
        stderr_buf.push(b'\n');
    }
    let cap_mib = MAX_OUTPUT_BYTES / (1024 * 1024);
    stderr_buf.extend_from_slice(
        format!(
            "[intendant] {stream} exceeded the {cap_mib} MiB cap; \
             {discarded} bytes were discarded"
        )
        .as_bytes(),
    );
}

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

/// True when `name` names a controller-held credential that must not reach
/// the runtime. Most `INTENDANT_*` names are the controller→runtime control
/// channel (the mock-provider e2e rig rides `PROVIDER` + `INTENDANT_MOCK_*`
/// into children); explicitly catalogued credential names remain scrubbed.
///
/// Classification is done on the ASCII-uppercased name: Windows environment
/// names are case-insensitive (`%mistral_api_key%` and `%MISTRAL_API_KEY%`
/// resolve identically inside the runtime's shells), and dotenvy preserves
/// whatever casing the `.env` file used — a lowercase spelling must not
/// slip past the scrub.
fn is_provider_credential_env(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    if crate::credential_leases::is_dns_credential_env(&name) {
        return true;
    }
    if name.starts_with("INTENDANT_") {
        return false;
    }
    crate::provider::PROVIDER_KEY_ENV_VARS.contains(&name.as_str())
        || EXTRA_PROVIDER_CREDENTIAL_ENV_VARS.contains(&name.as_str())
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
        .chain(crate::credential_leases::DNS_CREDENTIAL_ENV_VARS.iter())
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

/// Convert captured child output to a `String` without copying: the buffer
/// is moved when it is valid UTF-8 (the overwhelmingly common case) and only
/// the invalid-UTF-8 path pays a lossy re-allocation.
fn output_buf_into_string(buf: Vec<u8>) -> String {
    String::from_utf8(buf).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// True when `line` is a complete runtime protocol line: JSON with a
/// `type` of `status`/`result` and a numeric `nonce` — the shape
/// `map_results_to_tool_responses` folds into per-command results.
fn is_runtime_protocol_line(line: &str) -> bool {
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
}

fn has_parseable_runtime_output(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout)
        .lines()
        .any(is_runtime_protocol_line)
}

fn output_with_exit_status(
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
    status: ExitStatus,
) -> Result<AgentOutput, CallerError> {
    // Failure triage borrows the buffers; the success path then MOVES them
    // into the returned strings (this used to memcpy the full — up to 64 MB —
    // output through `from_utf8_lossy(..).to_string()` on every batch).
    if !status.success() && !has_parseable_runtime_output(&stdout_buf) {
        let stderr = output_buf_into_string(stderr_buf);
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
    let stdout = output_buf_into_string(stdout_buf);
    let mut stderr = output_buf_into_string(stderr_buf);
    if !status.success() {
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
///
/// `has_ask_human` selects the no-timeout path for batches containing
/// `askHuman` (which polls indefinitely for the user). The caller derives it
/// once per batch (`tool_batch::BatchFacts`) — this function used to re-parse
/// the entire batch JSON, including full editFile payloads, to answer it.
pub async fn run_agent(
    json_input: &str,
    log_dir: &std::path::Path,
    workdir: &std::path::Path,
    user_display_granted: bool,
    has_ask_human: bool,
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
                has_ask_human,
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
        has_ask_human,
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
    has_ask_human: bool,
) -> Result<AgentOutput, CallerError> {
    run_agent_inner(
        json_input,
        log_dir,
        None,
        Some(sandbox),
        user_display_granted,
        has_ask_human,
    )
    .await
}

async fn run_agent_inner(
    json_input: &str,
    log_dir: &std::path::Path,
    workdir: Option<&std::path::Path>,
    sandbox: Option<&crate::sandbox::SandboxConfig>,
    user_display_granted: bool,
    has_ask_human: bool,
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
    let hard_timeout_secs: u64 = if has_ask_human { u64::MAX / 2 } else { 120 };
    let hard_timeout = Duration::from_secs(hard_timeout_secs);

    // Read stdout and stderr (bounded), then wait for exit, all under a
    // single hard timeout. The buffers live outside the timed future so a
    // hard-timeout kill can still salvage the results of commands that
    // completed before the deadline — the runtime streams each command's
    // JSONL result line as that command finishes.
    let mut stdout_buf: Vec<u8> = Vec::with_capacity(8192);
    let mut stderr_buf: Vec<u8> = Vec::with_capacity(8192);

    let result = timeout(hard_timeout, async {
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();

        // The reads cap what is buffered but always consume to EOF
        // (read_capped_then_drain) — an unread pipe would block the child's
        // writes and park `child.wait()` in this join forever.
        let read_stdout = async {
            match stdout {
                Some(ref mut out) => {
                    read_capped_then_drain(out, MAX_OUTPUT_BYTES as u64, &mut stdout_buf).await
                }
                None => Ok(0),
            }
        };
        let read_stderr = async {
            match stderr {
                Some(ref mut err) => {
                    read_capped_then_drain(err, MAX_OUTPUT_BYTES as u64, &mut stderr_buf).await
                }
                None => Ok(0),
            }
        };

        let (stdout_res, stderr_res, status) = tokio::join!(read_stdout, read_stderr, child.wait());
        let stdout_discarded = stdout_res?;
        let stderr_discarded = stderr_res?;
        Ok::<_, CallerError>((status?, stdout_discarded, stderr_discarded))
    })
    .await;

    match result {
        Ok(Ok((status, stdout_discarded, stderr_discarded))) => {
            append_over_cap_note(&mut stderr_buf, "stdout", stdout_discarded);
            append_over_cap_note(&mut stderr_buf, "stderr", stderr_discarded);
            output_with_exit_status(stdout_buf, stderr_buf, status)
        }
        Ok(Err(err)) => Err(err),
        Err(_) => {
            let _ = child.kill().await;
            // Everything that finished before the deadline is intact JSONL
            // in the buffer. Salvage it instead of discarding completed
            // work the model would just redo; commands with no result line
            // surface as missing downstream.
            if let Some(salvaged) = salvage_partial_stdout(stdout_buf) {
                let stdout = output_buf_into_string(salvaged);
                let mut stderr = output_buf_into_string(stderr_buf);
                if !stderr.ends_with('\n') && !stderr.is_empty() {
                    stderr.push('\n');
                }
                stderr.push_str(&format!(
                    "sandboxed runtime killed after the {hard_timeout_secs}s batch hard-timeout; \
                     results above are from the commands that completed before the deadline"
                ));
                Ok(AgentOutput { stdout, stderr })
            } else {
                Err(CallerError::Agent(format!(
                    "Agent timed out after {}s",
                    hard_timeout_secs
                )))
            }
        }
    }
}

/// Prepare a timed-out batch's stdout for salvage. The kill can land
/// mid-write, and the result mapper folds any unparseable line into
/// ordinary output text, so a torn trailing fragment must not survive —
/// but the trailing bytes are not always torn: the runtime's stdout is a
/// line-buffered writer whose ~1 KiB buffer flushes a large result JSON to
/// the pipe *before* the separate newline write, so a kill in that window
/// leaves a COMPLETE result for a command that already ran (possibly with
/// side effects) — dropping it would make the model redo the command.
/// Keep the post-newline tail iff it parses as a complete protocol line;
/// drop it otherwise. Returns None when nothing parseable remains (the
/// batch keeps its timeout error).
fn salvage_partial_stdout(mut stdout_buf: Vec<u8>) -> Option<Vec<u8>> {
    let cut = stdout_buf
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0);
    let tail_is_complete_line = std::str::from_utf8(&stdout_buf[cut..])
        .is_ok_and(|tail| !tail.is_empty() && is_runtime_protocol_line(tail));
    if !tail_is_complete_line {
        stdout_buf.truncate(cut);
    }
    if has_parseable_runtime_output(&stdout_buf) {
        Some(stdout_buf)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Valid UTF-8 moves through without a copy; invalid UTF-8 falls back
    /// to lossy replacement (the pre-move behavior for both cases).
    #[test]
    fn output_buf_into_string_moves_utf8_and_lossy_falls_back() {
        assert_eq!(
            output_buf_into_string(b"plain output".to_vec()),
            "plain output"
        );
        let mut broken = b"tail: ".to_vec();
        broken.push(0xFF);
        assert_eq!(output_buf_into_string(broken), "tail: \u{FFFD}");
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
            "CLOUDFLARE_API_TOKEN",
            "INTENDANT_RFC2136_TSIG_SECRET",
            "OWNER_DNS_TSIG_SECRET",
            // Windows env names are case-insensitive and dotenvy preserves
            // the .env file's casing — %mistral_api_key% resolves as
            // %MISTRAL_API_KEY% inside the runtime, so casing must not
            // dodge the scrub.
            "mistral_api_key",
            "Anthropic_Api_Key",
            "openai_api_key",
            "custom_api_token",
            "anthropic_auth_token",
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
            "INTENDANT_FAKE_API_KEY", // non-credential INTENDANT_* controls survive
            "intendant_fake_api_key", // …in any casing
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
                "CLOUDFLARE_API_TOKEN",
                "INTENDANT_RFC2136_TSIG_SECRET",
                "OWNER_DNS_TSIG_SECRET",
                "mistral_api_key",
                "Custom_Api_Token",
                "PATH",
                "HOME",
                "PROVIDER",
                "INTENDANT_MOCK_SCRIPT",
            ],
        );
        let envs: Vec<(OsString, Option<OsString>)> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
            .collect();
        // Windows' Command env map is case-insensitive (case-variant keys
        // collapse into one entry), so match names case-insensitively.
        let removal_entry_for = |name: &str| {
            envs.iter()
                .any(|(k, v)| v.is_none() && k.to_string_lossy().eq_ignore_ascii_case(name))
        };
        let any_entry_for = |name: &str| {
            envs.iter()
                .any(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case(name))
        };

        // Removed vars appear as explicit (name, None) child-env entries;
        // mixed-case inherited names are removed too.
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "MISTRAL_API_KEY",
            "CUSTOM_API_TOKEN",
            "CLOUDFLARE_API_TOKEN",
            "INTENDANT_RFC2136_TSIG_SECRET",
            "OWNER_DNS_TSIG_SECRET",
            "mistral_api_key",
            "Custom_Api_Token",
        ] {
            assert!(
                removal_entry_for(name),
                "{name} must be removed from the child env"
            );
        }
        // Preserved vars have no explicit entry at all: they inherit.
        for name in ["PATH", "HOME", "PROVIDER", "INTENDANT_MOCK_SCRIPT"] {
            assert!(
                !any_entry_for(name),
                "{name} must inherit untouched (no explicit entry)"
            );
        }
        assert!(
            envs.iter()
                .any(|(k, v)| k == OsStr::new("INTENDANT_LOG_DIR")
                    && v.as_deref() == Some(OsStr::new("/tmp/logs"))),
            "runtime control vars set at the spawn boundary must survive the scrub"
        );
    }

    /// Timeout salvage must drop a torn trailing fragment (a mid-write kill
    /// otherwise leaks malformed JSON into the tool responses as ordinary
    /// text) while keeping BOTH every newline-terminated result and a
    /// complete trailing result whose newline never made it out — the
    /// line-buffered writer flushes a large payload to the pipe before the
    /// separate newline write, and that command already ran (possibly with
    /// side effects), so dropping it would make the model redo it.
    #[test]
    fn salvage_keeps_complete_results_drops_torn_fragments() {
        let first = br#"{"type":"result","nonce":1,"data":"first ok"}"#;
        let second_complete = br#"{"type":"result","nonce":2,"data":"second ok"}"#;

        // Torn second result: fragment dropped, first kept.
        let mut buf = first.to_vec();
        buf.push(b'\n');
        buf.extend_from_slice(br#"{"type":"result","nonce":2,"da"#); // killed mid-write
        let text = String::from_utf8(salvage_partial_stdout(buf).unwrap()).unwrap();
        assert!(text.contains("first ok"));
        assert!(text.ends_with('\n'));
        assert!(
            !text.contains(r#""nonce":2"#),
            "no fragment of the torn second result may remain: {text}"
        );

        // Complete second result missing only its newline: kept.
        let mut buf = first.to_vec();
        buf.push(b'\n');
        buf.extend_from_slice(second_complete);
        let text = String::from_utf8(salvage_partial_stdout(buf).unwrap()).unwrap();
        assert!(text.contains("first ok"));
        assert!(
            text.contains("second ok"),
            "a complete newline-less trailing result must survive: {text}"
        );

        // A lone complete result with no newline at all: also kept.
        let text = String::from_utf8(salvage_partial_stdout(first.to_vec()).unwrap()).unwrap();
        assert!(text.contains("first ok"));

        // Only a torn line: nothing to salvage.
        assert!(salvage_partial_stdout(br#"{"type":"result","non"#.to_vec()).is_none());

        // Unparseable noise lines alone don't qualify for salvage.
        assert!(salvage_partial_stdout(b"panic: something\n".to_vec()).is_none());

        // Noise plus a torn tail: still nothing parseable.
        assert!(salvage_partial_stdout(b"noise\n{\"type\":\"res".to_vec()).is_none());
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

    /// A small injected cap for the drain tests — never allocate the real
    /// 64 MiB in a unit test.
    const TEST_CAP: u64 = 1024;

    /// Run one writer/reader pair over an in-memory duplex whose internal
    /// buffer is far smaller than the payload, so an unconsumed reader
    /// wedges the writer exactly like a full OS pipe. Returns
    /// (buffered bytes, discarded count). The surrounding timeout turns the
    /// deadlock regression into a fast failure instead of a hung test.
    async fn drain_round_trip(total: usize, cap: u64) -> (Vec<u8>, u64) {
        let (mut reader, mut writer) = tokio::io::duplex(64);
        let payload = vec![0xAB_u8; total];
        let write_side = async move {
            writer.write_all(&payload).await.unwrap();
            // writer dropped here → EOF
        };
        let mut buf = Vec::new();
        let read_side = read_capped_then_drain(&mut reader, cap, &mut buf);
        let (_, read_res) = timeout(Duration::from_secs(30), async {
            tokio::join!(write_side, read_side)
        })
        .await
        .expect("writer or reader deadlocked: over-cap output was not drained");
        let discarded = read_res.unwrap();
        (buf, discarded)
    }

    /// The deadlock regression: a child writing more than the cap must
    /// still reach EOF (the writer future completes) because the remainder
    /// is drained, while the buffer holds exactly the cap and the discard
    /// count is reported for the truncation note.
    #[tokio::test]
    async fn over_cap_output_is_drained_so_the_writer_completes() {
        let total = TEST_CAP as usize * 4 + 37;
        let (buf, discarded) = drain_round_trip(total, TEST_CAP).await;
        assert_eq!(buf.len() as u64, TEST_CAP, "buffer must stop at the cap");
        assert_eq!(discarded, total as u64 - TEST_CAP);
    }

    /// Output landing exactly at the cap is complete, not truncated: the
    /// drain finds immediate EOF and no discard is reported (so no
    /// truncation note is emitted downstream).
    #[tokio::test]
    async fn exactly_at_cap_output_reports_no_discard() {
        let (buf, discarded) = drain_round_trip(TEST_CAP as usize, TEST_CAP).await;
        assert_eq!(buf.len() as u64, TEST_CAP);
        assert_eq!(discarded, 0, "exactly-at-cap output must not be flagged");
        assert!(buf.iter().all(|&b| b == 0xAB));
    }

    /// Under-cap output reads normally end to end with nothing discarded.
    #[tokio::test]
    async fn under_cap_output_reads_fully_with_no_discard() {
        let total = 100;
        let (buf, discarded) = drain_round_trip(total, TEST_CAP).await;
        assert_eq!(buf.len(), total, "under-cap output must arrive intact");
        assert_eq!(discarded, 0);
    }

    /// The truncation note is appended only when bytes were discarded, on
    /// its own line, naming the stream and the count; untouched streams
    /// (including exactly-at-cap, discarded == 0) add nothing.
    #[test]
    fn over_cap_note_appends_only_for_discarded_bytes() {
        let cap_mib = MAX_OUTPUT_BYTES / (1024 * 1024);

        // discarded == 0 leaves the buffer untouched.
        let mut stderr_buf = b"child stderr".to_vec();
        append_over_cap_note(&mut stderr_buf, "stdout", 0);
        assert_eq!(stderr_buf, b"child stderr");

        // A note on a non-empty buffer lands on its own line.
        append_over_cap_note(&mut stderr_buf, "stdout", 5);
        assert_eq!(
            String::from_utf8(stderr_buf.clone()).unwrap(),
            format!(
                "child stderr\n[intendant] stdout exceeded the {cap_mib} MiB cap; \
                 5 bytes were discarded"
            )
        );

        // A second stream's note separates from the first.
        append_over_cap_note(&mut stderr_buf, "stderr", 7);
        assert!(String::from_utf8(stderr_buf).unwrap().ends_with(&format!(
            "\n[intendant] stderr exceeded the {cap_mib} MiB cap; 7 bytes were discarded"
        )));

        // An empty buffer takes the note without a leading newline.
        let mut empty = Vec::new();
        append_over_cap_note(&mut empty, "stdout", 1);
        assert_eq!(
            String::from_utf8(empty).unwrap(),
            format!("[intendant] stdout exceeded the {cap_mib} MiB cap; 1 bytes were discarded")
        );
    }
}
