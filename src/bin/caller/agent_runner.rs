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
const RUNTIME_PROTOCOL_TOKEN_FIELD: &str = "runtime_protocol_token";

/// Per-batch askHuman answer tokens, keyed by the batch's log dir. The
/// runtime only accepts a `human_response` file whose first line matches
/// the batch's token (see `models::AgentInput::human_response_token`), so
/// a model-driven shell — which can write the answer path — cannot forge
/// an answer. The token lives only here and in the spawned runtime's
/// memory; frontends prepend it via [`write_human_response`].
fn human_response_tokens() -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, String>>
{
    static TOKENS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, String>>,
    > = std::sync::OnceLock::new();
    TOKENS.get_or_init(Default::default)
}

/// The registry key for a batch's log dir: canonicalized so the arm site
/// (agent loop) and the answer writers (frontends) agree even when one of
/// them holds a symlinked spelling (macOS `/var` vs `/private/var`).
fn human_response_token_key(log_dir: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(log_dir).unwrap_or_else(|_| log_dir.to_path_buf())
}

/// Per-session sandbox write grants from the denial-consent flow's
/// "allow for this session" resolution, keyed like the token registry.
/// Merged into the write set at every runtime spawn for that session;
/// gone on daemon restart by design.
fn session_write_grants(
) -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, Vec<PathBuf>>> {
    static GRANTS: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<PathBuf, Vec<PathBuf>>>,
    > = std::sync::OnceLock::new();
    GRANTS.get_or_init(Default::default)
}

/// Record a session-scoped write grant (consent card: "allow for this
/// session").
pub(crate) fn add_session_write_grant(log_dir: &std::path::Path, path: &std::path::Path) {
    let mut grants = session_write_grants()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let entry = grants.entry(human_response_token_key(log_dir)).or_default();
    if !entry.iter().any(|p| p == path) {
        entry.push(path.to_path_buf());
    }
}

/// The session's consent-granted write paths.
pub(crate) fn session_write_grants_for(log_dir: &std::path::Path) -> Vec<PathBuf> {
    session_write_grants()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&human_response_token_key(log_dir))
        .cloned()
        .unwrap_or_default()
}

/// Removes the batch's token entry when the runtime spawn returns.
struct HumanResponseTokenGuard {
    log_dir: PathBuf,
}

impl Drop for HumanResponseTokenGuard {
    fn drop(&mut self) {
        human_response_tokens()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.log_dir);
    }
}

/// Mint and register this batch's askHuman token, returning the batch
/// JSON with the token field set (any model-supplied value is
/// overwritten — the model must never know the accepted token) plus the
/// registry guard. Unparseable JSON passes through untouched: the runtime
/// will reject it with its own parse diagnostics.
fn arm_human_response_token(
    json_input: &str,
    log_dir: &std::path::Path,
) -> (Option<String>, Option<HumanResponseTokenGuard>) {
    let mut parsed: serde_json::Value = match serde_json::from_str(json_input) {
        Ok(value) => value,
        Err(_) => return (None, None),
    };
    let Some(map) = parsed.as_object_mut() else {
        return (None, None);
    };
    let token = uuid::Uuid::new_v4().simple().to_string();
    map.insert(
        "human_response_token".to_string(),
        serde_json::Value::String(token.clone()),
    );
    let key = human_response_token_key(log_dir);
    human_response_tokens()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key.clone(), token);
    (
        Some(parsed.to_string()),
        Some(HumanResponseTokenGuard { log_dir: key }),
    )
}

/// Mint a per-spawn secret for authenticating the runtime's stdout
/// protocol. Any model-supplied value is overwritten before the JSON is
/// written to the runtime's one-shot stdin.
fn arm_runtime_protocol_token(json_input: &str) -> (Option<String>, Option<String>) {
    let mut parsed: serde_json::Value = match serde_json::from_str(json_input) {
        Ok(value) => value,
        Err(_) => return (None, None),
    };
    let Some(map) = parsed.as_object_mut() else {
        return (None, None);
    };
    let token = uuid::Uuid::new_v4().simple().to_string();
    map.insert(
        RUNTIME_PROTOCOL_TOKEN_FIELD.to_string(),
        serde_json::Value::String(token.clone()),
    );
    (Some(parsed.to_string()), Some(token))
}

/// Write an askHuman answer the runtime will accept: the batch token (when
/// one is armed for `log_dir`) on the first line, then the answer text.
/// The single writer helper for every frontend answer path.
pub(crate) fn write_human_response(log_dir: &std::path::Path, text: &str) -> std::io::Result<()> {
    let token = human_response_tokens()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&human_response_token_key(log_dir))
        .cloned();
    let payload = match token {
        Some(token) => format!("{token}\n{text}"),
        None => text.to_string(),
    };
    std::fs::write(log_dir.join("human_response"), payload.as_bytes())
}

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

/// True when `name` names a provider/model-API or DNS credential that must
/// not reach model-driven children. This predicate otherwise reserves
/// `INTENDANT_*` because legitimate controller control names may use
/// credential-looking suffixes; it is not an admission rule. The native
/// runtime's allowlist default-denies that namespace except for exact
/// catalogued or spawn-injected controls.
///
/// Classification is done on the ASCII-uppercased name: Windows environment
/// names are case-insensitive (`%mistral_api_key%` and `%MISTRAL_API_KEY%`
/// resolve identically inside the runtime's shells), and dotenvy preserves
/// whatever casing the `.env` file used — a lowercase spelling must not
/// slip past the scrub.
///
/// `pub(crate)` because the external-agent spawn boundary
/// (`external_agent::external_child_env_allowed`) enforces the same
/// never-pass rule for supervised CLIs.
pub(crate) fn is_provider_credential_env(name: &str) -> bool {
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

/// Controller variables that may cross into the native runtime by default.
///
/// This is deliberately narrower than the external-agent allowlist:
/// provider CLIs need their own config-home and proxy settings, while the
/// runtime is the untrusted command executor and must not inherit either.
/// Keep entries to OS/process essentials and non-secret toolchain controls.
/// `RUSTC_WRAPPER` and `RUSTC` are load-bearing on developer/CI hosts where
/// the compile governor is installed through those variables.
const RUNTIME_CHILD_BASE_ENV: &[&str] = &[
    // System basics (all platforms).
    "PATH",
    "HOME",
    "USER",
    "USERNAME",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TEMP",
    "TMP",
    "TERM",
    "COLORTERM",
    "TZ",
    "LANG",
    "NO_COLOR",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "FORCE_COLOR",
    // macOS.
    "__CF_USER_TEXT_ENCODING",
    "MACOSX_DEPLOYMENT_TARGET",
    "SDKROOT",
    "DEVELOPER_DIR",
    // Linux/Unix display and desktop session. The Linux spawn path also
    // refreshes these explicitly through `linux_display_env`.
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "XDG_RUNTIME_DIR",
    "XDG_DATA_DIRS",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_SESSION_TYPE",
    "XDG_CURRENT_DESKTOP",
    "DESKTOP_SESSION",
    // Windows process/DLL/profile discovery.
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
    "COMPUTERNAME",
    "USERDOMAIN",
    // Common language/build toolchain paths and non-secret controls.
    "CARGO_HOME",
    "RUSTUP_HOME",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_JOBS",
    "CARGO_INCREMENTAL",
    "CARGO_TERM_COLOR",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "SCCACHE_DIR",
    "SCCACHE_SERVER_PORT",
    "SCCACHE_IDLE_TIMEOUT",
    "SCCACHE_NO_DAEMON",
    "JAVA_HOME",
    "MAVEN_HOME",
    "GRADLE_USER_HOME",
    "GOPATH",
    "GOROOT",
    "GOMODCACHE",
    "NVM_DIR",
    "PNPM_HOME",
    "BUN_INSTALL",
    "VIRTUAL_ENV",
    "UV_CACHE_DIR",
    "CC",
    "CXX",
    "AR",
    "CFLAGS",
    "CPPFLAGS",
    "CXXFLAGS",
    "LDFLAGS",
    "PKG_CONFIG_PATH",
    "PKG_CONFIG_LIBDIR",
    "PKG_CONFIG_SYSROOT_DIR",
    // Exact controller→runtime path control; all other INTENDANT_* names
    // default-deny and the spawn site injects its live controls explicitly.
    "INTENDANT_HOME",
];

/// Whether a controller env name may be copied into the runtime's cleared
/// environment. Matching is case-insensitive for Windows parity.
///
/// `passthrough` is the deliberate exact-name extension from
/// `INTENDANT_ENV_PASSTHROUGH`. It can admit otherwise unknown or
/// credential-like host variables, but provider/model API keys are an
/// absolute deny even when named.
fn runtime_child_env_allowed(name: &str, passthrough: &std::collections::HashSet<String>) -> bool {
    let upper = name.to_ascii_uppercase();
    if is_provider_credential_env(&upper) {
        return false;
    }
    passthrough.contains(&upper)
        || RUNTIME_CHILD_BASE_ENV.contains(&upper.as_str())
        || upper.starts_with("LC_")
}

/// The exact inherited pairs admitted by [`runtime_child_env_allowed`].
fn runtime_child_env_pairs(
    inherited: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    passthrough: &std::collections::HashSet<String>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    inherited
        .into_iter()
        .filter(|(name, _)| {
            name.to_str()
                .is_some_and(|name| runtime_child_env_allowed(name, passthrough))
        })
        .collect()
}

/// Reset a native runtime child's environment to the explicit allowlist.
///
/// Call before the spawn site's deliberate `.env()` injections so the log
/// root, sandbox grants, user-display grant, and refreshed Linux GUI values
/// are derived after the clear and cannot be supplied by ambient process
/// state.
fn apply_runtime_child_env_policy(command: &mut Command) {
    let passthrough = intendant_core::env_scrub::env_passthrough_set(
        std::env::var(intendant_core::env_scrub::ENV_PASSTHROUGH_VAR)
            .ok()
            .as_deref(),
    );
    apply_runtime_child_env_policy_from(command, std::env::vars_os(), &passthrough);
}

/// Testable core of [`apply_runtime_child_env_policy`].
fn apply_runtime_child_env_policy_from(
    command: &mut Command,
    inherited: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    passthrough: &std::collections::HashSet<String>,
) {
    command.env_clear();
    for (name, value) in runtime_child_env_pairs(inherited, passthrough) {
        command.env(name, value);
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

/// Authenticate and normalize one runtime protocol line, stripping the
/// internal token before the line can reach result mapping or session logs.
fn authenticated_runtime_protocol_line(line: &str, expected_token: &str) -> Option<String> {
    let mut parsed = serde_json::from_str::<serde_json::Value>(line.trim()).ok()?;
    if !matches!(
        parsed.get("type").and_then(|value| value.as_str()),
        Some("status" | "result")
    ) || parsed
        .get("nonce")
        .and_then(|value| value.as_u64())
        .is_none()
        || parsed
            .get(RUNTIME_PROTOCOL_TOKEN_FIELD)
            .and_then(|value| value.as_str())
            != Some(expected_token)
    {
        return None;
    }
    parsed.as_object_mut()?.remove(RUNTIME_PROTOCOL_TOKEN_FIELD);
    Some(parsed.to_string())
}

/// Keep only complete, authenticated protocol records. Runtime stdout is
/// otherwise untrusted: a model-driven descendant may find a writable
/// alias for the controller pipe even though its ordinary stdout is a log
/// file. With no expected token, retain the legacy behavior used only when
/// the input was not a valid AgentInput object and no token could be armed.
fn authenticate_runtime_stdout(stdout: Vec<u8>, expected_token: Option<&str>) -> (Vec<u8>, usize) {
    let Some(expected_token) = expected_token else {
        return (stdout, 0);
    };

    let mut authenticated = Vec::with_capacity(stdout.len().min(8192));
    let mut rejected = 0usize;
    for raw_line in stdout.split(|byte| *byte == b'\n') {
        if raw_line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Some(line) = std::str::from_utf8(raw_line)
            .ok()
            .and_then(|line| authenticated_runtime_protocol_line(line, expected_token))
        else {
            rejected = rejected.saturating_add(1);
            continue;
        };
        authenticated.extend_from_slice(line.as_bytes());
        authenticated.push(b'\n');
    }
    (authenticated, rejected)
}

fn append_protocol_rejection_note(stderr: &mut Vec<u8>, rejected: usize) {
    if rejected == 0 {
        return;
    }
    if !stderr.is_empty() && !stderr.ends_with(b"\n") {
        stderr.push(b'\n');
    }
    stderr.extend_from_slice(
        format!("discarded {rejected} unauthenticated or malformed runtime stdout line(s)")
            .as_bytes(),
    );
}

fn has_parseable_runtime_output(stdout: &[u8]) -> bool {
    String::from_utf8_lossy(stdout)
        .lines()
        .any(is_runtime_protocol_line)
}

fn output_with_exit_status(
    stdout_buf: Vec<u8>,
    mut stderr_buf: Vec<u8>,
    status: ExitStatus,
    expected_token: Option<&str>,
) -> Result<AgentOutput, CallerError> {
    let raw_had_output = stdout_buf.iter().any(|byte| !byte.is_ascii_whitespace());
    let (stdout_buf, rejected) = authenticate_runtime_stdout(stdout_buf, expected_token);
    append_protocol_rejection_note(&mut stderr_buf, rejected);

    if expected_token.is_some() && raw_had_output && !has_parseable_runtime_output(&stdout_buf) {
        let stderr = output_buf_into_string(stderr_buf);
        let detail = if stderr.trim().is_empty() {
            String::new()
        } else {
            format!("; stderr: {}", stderr.trim())
        };
        return Err(CallerError::Agent(format!(
            "sandboxed runtime produced no authenticated protocol output{detail}"
        )));
    }

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
        let mut write_paths: Vec<PathBuf> = std::env::split_paths(&raw_paths)
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        if !write_paths.is_empty() {
            // Per-spawn additions beyond the daemon-launch grant set: the
            // session's own project root (a session's project can differ
            // from the daemon's launch project — dashboard-picked
            // projects, projectless daemons) plus any consent-flow
            // session grants.
            let mut session_paths: Vec<PathBuf> = Vec::new();
            if !write_paths.iter().any(|p| p == workdir) {
                session_paths.push(workdir.to_path_buf());
            }
            for grant in session_write_grants_for(log_dir) {
                if !write_paths.iter().any(|p| p == &grant) {
                    session_paths.push(grant);
                }
            }
            write_paths.extend(session_paths.iter().cloned());
            // Windows write grants are ACE stamps on the target dirs.
            // Stamp the WHOLE effective set per spawn, not just the
            // session additions: paths granted live after startup (the
            // consent flow's "always allow", a settings save) arrive via
            // the env var and would otherwise never get an ACE. The
            // refcounted GRANTS table makes this cheap — startup-held
            // paths just bump their count (no DACL write); only genuinely
            // new paths pay the 0→1 stamp, and the guard's Drop returns
            // them to 0 when the child exits.
            #[cfg(windows)]
            let _session_ace = Some(
                crate::win_sandbox::AceGuard::stamp(&[], &write_paths)
                    .map_err(|e| CallerError::Agent(format!("stamp session write grant: {e}")))?,
            );
            #[cfg(not(windows))]
            let _ = &session_paths;
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
    // Authenticate every result line with a fresh, controller-minted
    // secret. Arm askHuman after it so both internal fields ride the same
    // one-shot stdin payload; neither is exposed through the child shell's
    // environment.
    let (protocol_input, protocol_token) = arm_runtime_protocol_token(json_input);
    let protocol_input = protocol_input.as_deref().unwrap_or(json_input);
    let (human_input, _human_token_guard) = if has_ask_human {
        arm_human_response_token(protocol_input, log_dir)
    } else {
        (None, None)
    };
    let json_input = human_input.as_deref().unwrap_or(protocol_input);

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

    apply_runtime_child_env_policy(&mut cmd);
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
            output_with_exit_status(stdout_buf, stderr_buf, status, protocol_token.as_deref())
        }
        Ok(Err(err)) => Err(err),
        Err(_) => {
            let _ = child.kill().await;
            // Everything that finished before the deadline is intact JSONL
            // in the buffer. Salvage it instead of discarding completed
            // work the model would just redo; commands with no result line
            // surface as missing downstream.
            let (stdout_buf, rejected) =
                authenticate_runtime_stdout(stdout_buf, protocol_token.as_deref());
            append_protocol_rejection_note(&mut stderr_buf, rejected);
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

    /// The armed token overwrites any model-supplied value, the writer
    /// helper prefixes it, and dropping the guard reverts the writer to
    /// bare (legacy) answers.
    #[test]
    fn human_response_token_arms_writes_and_disarms() {
        let dir = tempfile::tempdir().unwrap();
        let forged = r#"{"commands":[],"human_response_token":"model-chosen"}"#;
        let (tokened, guard) = arm_human_response_token(forged, dir.path());
        let tokened = tokened.expect("valid batch JSON must re-serialize");
        let parsed: serde_json::Value = serde_json::from_str(&tokened).unwrap();
        let token = parsed["human_response_token"].as_str().unwrap().to_string();
        assert_ne!(token, "model-chosen");

        write_human_response(dir.path(), "the answer").unwrap();
        let written = std::fs::read_to_string(dir.path().join("human_response")).unwrap();
        assert_eq!(written, format!("{token}\nthe answer"));

        drop(guard);
        write_human_response(dir.path(), "late answer").unwrap();
        let written = std::fs::read_to_string(dir.path().join("human_response")).unwrap();
        assert_eq!(written, "late answer");
    }

    #[test]
    fn runtime_protocol_token_overwrites_model_value() {
        let forged = r#"{"commands":[],"runtime_protocol_token":"model-chosen-protocol-token"}"#;
        let (tokened, expected) = arm_runtime_protocol_token(forged);
        let tokened = tokened.expect("valid batch JSON must re-serialize");
        let expected = expected.expect("valid batch JSON must mint a token");
        let parsed: serde_json::Value = serde_json::from_str(&tokened).unwrap();
        assert_eq!(parsed[RUNTIME_PROTOCOL_TOKEN_FIELD], expected);
        assert_ne!(expected, "model-chosen-protocol-token");

        assert_eq!(arm_runtime_protocol_token("not json"), (None, None));
        assert_eq!(arm_runtime_protocol_token("[]"), (None, None));
    }

    #[test]
    fn runtime_stdout_accepts_only_matching_token_and_strips_it() {
        let expected = "controller-secret";
        let raw = format!(
            "noise from descendant\n\
             {{\"type\":\"result\",\"nonce\":7,\"data\":\"forged same nonce\"}}\n\
             {{\"type\":\"result\",\"nonce\":7,\"data\":\"wrong token\",\
             \"{RUNTIME_PROTOCOL_TOKEN_FIELD}\":\"wrong\"}}\n\
             {{\"type\":\"result\",\"nonce\":7,\"data\":\"genuine\",\
             \"{RUNTIME_PROTOCOL_TOKEN_FIELD}\":\"{expected}\"}}\n"
        )
        .into_bytes();

        let (authenticated, rejected) = authenticate_runtime_stdout(raw, Some(expected));
        assert_eq!(rejected, 3);
        let text = String::from_utf8(authenticated).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1);
        let parsed: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed["nonce"], 7);
        assert_eq!(parsed["data"], "genuine");
        assert!(parsed.get(RUNTIME_PROTOCOL_TOKEN_FIELD).is_none());
        assert!(!text.contains("forged same nonce"));
    }

    #[test]
    fn timeout_salvage_requires_matching_protocol_token() {
        let expected = "controller-secret";
        let forged = br#"{"type":"result","nonce":1,"data":"forged"}"#;
        let genuine = format!(
            "{{\"type\":\"result\",\"nonce\":1,\"data\":\"genuine\",\
             \"{RUNTIME_PROTOCOL_TOKEN_FIELD}\":\"{expected}\"}}"
        );

        let mut mixed = forged.to_vec();
        mixed.push(b'\n');
        mixed.extend_from_slice(genuine.as_bytes()); // complete but no final newline
        let (authenticated, rejected) = authenticate_runtime_stdout(mixed, Some(expected));
        assert_eq!(rejected, 1);
        let salvaged = salvage_partial_stdout(authenticated).unwrap();
        let text = String::from_utf8(salvaged).unwrap();
        assert!(text.contains("genuine"));
        assert!(!text.contains("forged"));
        assert!(!text.contains(RUNTIME_PROTOCOL_TOKEN_FIELD));

        let (authenticated, rejected) =
            authenticate_runtime_stdout(forged.to_vec(), Some(expected));
        assert_eq!(rejected, 1);
        assert!(salvage_partial_stdout(authenticated).is_none());
    }

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
            // The provider classifier reserves the INTENDANT_* namespace;
            // the runtime allowlist below still default-denies these.
            "INTENDANT_FAKE_API_KEY",
            "intendant_fake_api_key",
            "OPENAI_BASE_URL",
        ] {
            assert!(!is_provider_credential_env(name), "{name} must survive");
        }
    }

    /// The runtime allowlist admits only catalogued OS/toolchain names and
    /// locale variables. Secrets, arbitrary unknowns, proxy authority, and
    /// the rest of the INTENDANT_* namespace default-deny.
    #[test]
    fn runtime_child_env_allowlist_defaults_deny() {
        let none = std::collections::HashSet::new();
        for name in [
            "PATH",
            "HOME",
            "DISPLAY",
            "SystemRoot",
            "CARGO_TARGET_DIR",
            "RUSTC_WRAPPER",
            "SCCACHE_DIR",
            "JAVA_HOME",
            "VIRTUAL_ENV",
            "INTENDANT_HOME",
            "LANG",
            "LC_ALL",
            "lc_messages",
        ] {
            assert!(
                runtime_child_env_allowed(name, &none),
                "{name} must be admitted"
            );
        }
        for name in [
            "ANTHROPIC_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "DATABASE_URL",
            "DB_PASSWORD",
            "SSH_AUTH_SOCK",
            "DBUS_SESSION_BUS_ADDRESS",
            "HTTP_PROXY",
            "NODE_OPTIONS",
            "LD_PRELOAD",
            "PROVIDER",
            "INTENDANT_MOCK_SCRIPT",
            "INTENDANT_CONNECT_TOKEN",
            "INTENDANT_FAKE_PASSWORD",
            "SOME_RANDOM_VAR",
        ] {
            assert!(
                !runtime_child_env_allowed(name, &none),
                "{name} must default-deny"
            );
        }
    }

    /// The applier clears inheritance, copies only admitted pairs, and keeps
    /// deliberate runtime controls injected after the clear. The exact-name
    /// passthrough extends the allowlist but can never admit provider keys.
    #[test]
    fn runtime_child_env_policy_copies_only_allowed_pairs() {
        use std::ffi::{OsStr, OsString};

        let inherited: Vec<(OsString, OsString)> = [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/u"),
            ("RUSTC_WRAPPER", "/usr/local/bin/rustc-governor"),
            ("INTENDANT_HOME", "/state"),
            ("ANTHROPIC_API_KEY", "sk-provider"),
            ("CLOUDFLARE_API_TOKEN", "dns-provider"),
            ("INTENDANT_RFC2136_TSIG_SECRET", "dns-tsig"),
            ("AWS_SECRET_ACCESS_KEY", "aws-secret"),
            ("GITHUB_TOKEN", "ghp-secret"),
            ("DATABASE_URL", "postgres://secret"),
            ("DB_PASSWORD", "db-secret"),
            ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
            ("CUSTOM_BUILD_ROOT", "/opt/build"),
            ("PROVIDER", "mock"),
            ("INTENDANT_MOCK_SCRIPT", "/tmp/script.json"),
            ("INTENDANT_CONNECT_TOKEN", "root-secret"),
            ("SOME_RANDOM_VAR", "unknown"),
        ]
        .into_iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect();
        let passthrough = intendant_core::env_scrub::env_passthrough_set(Some(
            "SSH_AUTH_SOCK, CUSTOM_BUILD_ROOT, ANTHROPIC_API_KEY, \
             CLOUDFLARE_API_TOKEN, INTENDANT_RFC2136_TSIG_SECRET",
        ));
        let mut cmd = Command::new("true");
        cmd.env("SHOULD_BE_CLEARED", "before-policy");
        apply_runtime_child_env_policy_from(&mut cmd, inherited, &passthrough);
        cmd.env("INTENDANT_LOG_DIR", "/tmp/logs");

        let envs: Vec<(OsString, Option<OsString>)> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
            .collect();
        let value_for = |name: &str| {
            envs.iter()
                .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case(name))
                .and_then(|(_, value)| value.clone())
        };

        for (name, value) in [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/u"),
            ("RUSTC_WRAPPER", "/usr/local/bin/rustc-governor"),
            ("INTENDANT_HOME", "/state"),
            ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
            ("CUSTOM_BUILD_ROOT", "/opt/build"),
            ("INTENDANT_LOG_DIR", "/tmp/logs"),
        ] {
            assert_eq!(
                value_for(name),
                Some(OsString::from(value)),
                "{name} must cross with its value"
            );
        }
        for name in [
            "SHOULD_BE_CLEARED",
            "ANTHROPIC_API_KEY",
            "CLOUDFLARE_API_TOKEN",
            "INTENDANT_RFC2136_TSIG_SECRET",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "DATABASE_URL",
            "DB_PASSWORD",
            "PROVIDER",
            "INTENDANT_MOCK_SCRIPT",
            "INTENDANT_CONNECT_TOKEN",
            "SOME_RANDOM_VAR",
        ] {
            assert!(
                value_for(name).is_none(),
                "{name} must be absent from the child env"
            );
        }
        assert!(
            envs.iter()
                .all(|(key, value)| key != OsStr::new("SHOULD_BE_CLEARED") || value.is_none()),
            "env_clear must discard entries set before the policy"
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
