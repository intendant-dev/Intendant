//! Provider-neutral remote command jobs.
//!
//! The public contract is deliberately host-shaped (`cloud:<task-id>` is the
//! first backend), while the Codex Cloud attachment is only one transport.
//! Commands are argv arrays, never shell strings. A required Git revision and
//! clean-worktree default keep a caller from accidentally validating a
//! different source tree than the one it intended to send.

mod cache_relay;
mod scheduler;
pub(crate) mod source;

use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::sync::{mpsc, oneshot, watch};

const MAX_ARGS: usize = 128;
const MAX_ARG_BYTES: usize = 64 * 1024;
const MAX_ENV_VARS: usize = 64;
const MAX_ENV_BYTES: usize = 64 * 1024;
const MAX_CWD_BYTES: usize = 1024;
const MIN_TIMEOUT_S: u64 = 1;
const MAX_TIMEOUT_S: u64 = 3600;
const OUTPUT_LIMIT_BYTES: usize = 128 * 1024;
const OUTPUT_HEAD_BYTES: usize = 32 * 1024;
const OUTPUT_DRAIN_TIMEOUT_S: u64 = 5;
const HOME_JOB_CAP: usize = 128;
const WORKER_JOB_CAP: usize = 8;
const WORKER_SEND_TIMEOUT_S: u64 = 5;
const WORKER_WATCHDOG_GRACE_S: u64 = 60;
const CACHE_SERVER_IDLE_TIMEOUT_S: u64 = 600;
const WORKER_PRIVATE_STATE_ENV: &str = "INTENDANT_HOME";

pub(crate) const REMOTE_COMMAND_START_KIND: &str = "remote_command_start";
pub(crate) const REMOTE_COMMAND_CANCEL_KIND: &str = "remote_command_cancel";
pub(crate) const REMOTE_COMMAND_RESULT_KIND: &str = "remote_command_result";

pub(crate) fn route_remote_cache_frame(task_id: &str, text: &str) -> bool {
    cache_relay::route_worker_frame(task_id, text)
}

fn remove_worker_private_state(command: &mut tokio::process::Command) {
    command.env_remove(WORKER_PRIVATE_STATE_ENV);
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteSourceMode {
    #[default]
    GitRevision,
    WorkingTree,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteCacheMode {
    #[default]
    None,
    DurableSccache,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct RemoteCommandSpec {
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub expected_revision: String,
    /// Content-addressed source prepared through the separate bounded source
    /// transfer lane. Absent means execute the worker's provider checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(default)]
    pub cache: RemoteCacheMode,
    /// Dedicated cache credentials/config copied from prefixed home
    /// variables. This is transported over mTLS and never copied into job
    /// views or result logs.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    cache_env: BTreeMap<String, String>,
    /// Opaque, one-job capability for the attachment-backed home cache.
    /// It is never copied into job views, results, or debug output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_relay_token: Option<String>,
    #[serde(default = "default_require_clean")]
    pub require_clean: bool,
    pub timeout_s: u64,
}

impl std::fmt::Debug for RemoteCommandSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteCommandSpec")
            .field("argv", &self.argv)
            .field("cwd", &self.cwd)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("expected_revision", &self.expected_revision)
            .field("source_id", &self.source_id)
            .field("cache", &self.cache)
            .field("cache_env_keys", &self.cache_env.keys().collect::<Vec<_>>())
            .field("cache_relay", &self.cache_relay_token.is_some())
            .field("require_clean", &self.require_clean)
            .field("timeout_s", &self.timeout_s)
            .finish()
    }
}

fn default_require_clean() -> bool {
    true
}

impl RemoteCommandSpec {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.argv.is_empty() {
            return Err("argv must contain an executable".to_string());
        }
        if self.argv.len() > MAX_ARGS {
            return Err(format!("argv may contain at most {MAX_ARGS} entries"));
        }
        if self.argv[0].trim().is_empty() {
            return Err("argv[0] must name an executable".to_string());
        }
        let arg_bytes = self.argv.iter().try_fold(0usize, |total, arg| {
            if arg.contains('\0') {
                return Err("argv entries may not contain NUL bytes".to_string());
            }
            total
                .checked_add(arg.len())
                .ok_or_else(|| "argv is too large".to_string())
        })?;
        if arg_bytes > MAX_ARG_BYTES {
            return Err(format!(
                "argv may contain at most {MAX_ARG_BYTES} bytes in total"
            ));
        }
        let revision = self.expected_revision.trim();
        if !(7..=64).contains(&revision.len())
            || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(
                "expected_revision must be a 7-64 character hexadecimal Git object id".to_string(),
            );
        }
        if !(MIN_TIMEOUT_S..=MAX_TIMEOUT_S).contains(&self.timeout_s) {
            return Err(format!(
                "timeout_s must be between {MIN_TIMEOUT_S} and {MAX_TIMEOUT_S}"
            ));
        }
        if self.source_id.as_deref().is_some_and(|id| {
            !id.starts_with("source-")
                || id.len() != "source-".len() + 64
                || !id["source-".len()..]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err("source_id must be a content-addressed source digest".to_string());
        }
        if let Some(cwd) = self.cwd.as_deref() {
            validate_relative_cwd(cwd)?;
        }
        if self.env.len() > MAX_ENV_VARS {
            return Err(format!("env may contain at most {MAX_ENV_VARS} variables"));
        }
        let mut env_bytes = 0usize;
        for (key, value) in &self.env {
            if !valid_env_key(key) {
                return Err(format!("invalid environment variable name '{key}'"));
            }
            if value.contains('\0') {
                return Err(format!("environment variable '{key}' contains a NUL byte"));
            }
            env_bytes = env_bytes
                .checked_add(key.len())
                .and_then(|total| total.checked_add(value.len()))
                .ok_or_else(|| "env is too large".to_string())?;
        }
        if env_bytes > MAX_ENV_BYTES {
            return Err(format!(
                "env may contain at most {MAX_ENV_BYTES} bytes in total"
            ));
        }
        let has_direct_cache = !self.cache_env.is_empty();
        let has_home_relay = self.cache_relay_token.is_some();
        if self.cache == RemoteCacheMode::None && (has_direct_cache || has_home_relay) {
            return Err("cache configuration was supplied while cache mode is none".to_string());
        }
        if self.cache == RemoteCacheMode::DurableSccache && (has_direct_cache == has_home_relay) {
            return Err("durable_sccache requires exactly one durable cache transport".to_string());
        }
        if let Some(token) = self.cache_relay_token.as_deref() {
            if token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("durable cache relay capability has an invalid shape".to_string());
            }
        }
        let cache_env_bytes = self.cache_env.iter().try_fold(
            0usize,
            |total, (key, value)| -> Result<usize, String> {
                if !valid_env_key(key)
                    || !dedicated_cache_env_key(key)
                    || managed_cache_env_key(key)
                    || value.contains('\0')
                {
                    return Err(format!("invalid dedicated cache environment key '{key}'"));
                }
                total
                    .checked_add(key.len().saturating_add(value.len()))
                    .ok_or_else(|| "cache environment is too large".to_string())
            },
        )?;
        if self.cache_env.len() > MAX_ENV_VARS || cache_env_bytes > MAX_ENV_BYTES {
            return Err("dedicated cache environment exceeds the command limits".to_string());
        }
        Ok(())
    }
}

fn valid_env_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn dedicated_cache_env_key(key: &str) -> bool {
    [
        "SCCACHE_",
        "AWS_",
        "ACTIONS_",
        "ALIBABA_CLOUD_",
        "TENCENTCLOUD_",
    ]
    .iter()
    .any(|prefix| key.starts_with(prefix))
}

fn managed_cache_env_key(key: &str) -> bool {
    matches!(
        key,
        "SCCACHE_DIR"
            | "SCCACHE_SERVER_PORT"
            | "SCCACHE_SERVER_UDS"
            | "SCCACHE_IDLE_TIMEOUT"
            | "SCCACHE_BASEDIRS"
            | "SCCACHE_IGNORE_SERVER_IO_ERROR"
    )
}

fn validate_relative_cwd(raw: &str) -> Result<(), String> {
    if raw.is_empty() || raw.len() > MAX_CWD_BYTES {
        return Err(format!(
            "cwd must contain 1-{MAX_CWD_BYTES} bytes and be repository-relative"
        ));
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err("cwd must be repository-relative".to_string());
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err("cwd may not contain parent, root, or platform-prefix components".to_string());
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteCommandState {
    Acquiring,
    Preparing,
    Queued,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}

impl RemoteCommandState {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::TimedOut | Self::Cancelled
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RemoteCommandResult {
    pub state: RemoteCommandState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub stdout_truncated: bool,
    #[serde(default)]
    pub stderr_truncated: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_dirty_after: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<RemoteCacheReport>,
}

impl RemoteCommandResult {
    fn failed(error: impl Into<String>, started: Instant) -> Self {
        Self {
            state: RemoteCommandState::Failed,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            duration_ms: elapsed_ms(started),
            error: Some(error.into()),
            worker_revision: None,
            workspace_dirty_after: None,
            cache: None,
        }
    }

    fn cancelled(error: impl Into<String>, started: Instant) -> Self {
        let mut result = Self::failed(error, started);
        result.state = RemoteCommandState::Cancelled;
        result
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RemoteCacheReport {
    pub mode: RemoteCacheMode,
    pub hits_delta: u64,
    pub misses_delta: u64,
    pub writes_delta: u64,
    pub errors_delta: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RemoteCommandJobView {
    pub job_id: String,
    pub host: String,
    pub state: RemoteCommandState,
    pub program: String,
    pub arg_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub expected_revision: String,
    pub require_clean: bool,
    pub timeout_s: u64,
    pub source: RemoteSourceMode,
    pub cache: RemoteCacheMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_bytes: Option<u64>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<RemoteCommandResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum RemoteCommandCaller {
    Unrestricted,
    AgentSession(String),
}

impl RemoteCommandCaller {
    fn may_access(&self, owner_session_id: Option<&str>) -> bool {
        match self {
            Self::Unrestricted => true,
            Self::AgentSession(session_id) => owner_session_id == Some(session_id.as_str()),
        }
    }

    fn owner_session_id(&self) -> Option<String> {
        match self {
            Self::Unrestricted => None,
            Self::AgentSession(session_id) => Some(session_id.clone()),
        }
    }
}

struct StoredRemoteCommandJob {
    view: RemoteCommandJobView,
    owner_session_id: Option<String>,
    updates: watch::Sender<RemoteCommandJobView>,
}

#[derive(Default)]
struct HomeRemoteCommandRegistry {
    jobs: HashMap<String, StoredRemoteCommandJob>,
    order: VecDeque<String>,
}

static HOME_REMOTE_COMMANDS: OnceLock<Mutex<HomeRemoteCommandRegistry>> = OnceLock::new();

fn home_registry() -> &'static Mutex<HomeRemoteCommandRegistry> {
    HOME_REMOTE_COMMANDS.get_or_init(|| Mutex::new(HomeRemoteCommandRegistry::default()))
}

fn insert_home_job(
    view: RemoteCommandJobView,
    owner_session_id: Option<String>,
) -> Result<(), String> {
    let mut registry = home_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    while registry.jobs.len() >= HOME_JOB_CAP {
        let removable = registry.order.iter().position(|job_id| {
            registry
                .jobs
                .get(job_id)
                .is_some_and(|job| job.view.state.is_terminal())
        });
        let Some(position) = removable else {
            return Err(format!(
                "remote command registry is full ({HOME_JOB_CAP} active jobs)"
            ));
        };
        if let Some(job_id) = registry.order.remove(position) {
            registry.jobs.remove(&job_id);
        }
    }
    let (updates, _receiver) = watch::channel(view.clone());
    registry.order.push_back(view.job_id.clone());
    registry.jobs.insert(
        view.job_id.clone(),
        StoredRemoteCommandJob {
            view,
            owner_session_id,
            updates,
        },
    );
    Ok(())
}

#[cfg(test)]
fn remove_home_job(job_id: &str) {
    let mut registry = home_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.jobs.remove(job_id);
    if let Some(position) = registry
        .order
        .iter()
        .position(|candidate| candidate == job_id)
    {
        registry.order.remove(position);
    }
}

fn update_home_job(
    job_id: &str,
    update: impl FnOnce(&mut RemoteCommandJobView),
) -> Option<RemoteCommandJobView> {
    let mut registry = home_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let job = registry.jobs.get_mut(job_id)?;
    update(&mut job.view);
    job.view.updated_at_unix_ms = crate::codex_cloud::now_unix_ms();
    let view = job.view.clone();
    job.updates.send_replace(view.clone());
    Some(view)
}

fn read_home_job(
    job_id: &str,
    caller: &RemoteCommandCaller,
) -> Result<RemoteCommandJobView, String> {
    let registry = home_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let job = registry
        .jobs
        .get(job_id)
        .ok_or_else(|| "remote command job was not found".to_string())?;
    if !caller.may_access(job.owner_session_id.as_deref()) {
        return Err("remote command job was not found".to_string());
    }
    Ok(job.view.clone())
}

fn subscribe_home_job(
    job_id: &str,
    caller: &RemoteCommandCaller,
) -> Result<watch::Receiver<RemoteCommandJobView>, String> {
    let registry = home_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let job = registry
        .jobs
        .get(job_id)
        .ok_or_else(|| "remote command job was not found".to_string())?;
    if !caller.may_access(job.owner_session_id.as_deref()) {
        return Err("remote command job was not found".to_string());
    }
    Ok(job.updates.subscribe())
}

async fn start_remote_command(
    requested_host: Option<String>,
    spec: RemoteCommandSpec,
    cache_store: Option<cache_relay::HomeCacheStore>,
    source_mode: RemoteSourceMode,
    snapshot: Option<source::HomeSourceSnapshot>,
    branch_hint: Option<String>,
    caller: RemoteCommandCaller,
) -> Result<RemoteCommandJobView, String> {
    spec.validate()?;
    let requested_host = requested_host
        .map(|host| host.trim().to_string())
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "auto".to_string());
    if requested_host != "auto"
        && crate::codex_cloud_attach::cloud_host_task_id(&requested_host).is_none()
    {
        return Err("unsupported remote host; use auto or cloud:<codex-task-id>".to_string());
    }

    let job_id = format!("remote-{}", uuid::Uuid::new_v4());
    let now = crate::codex_cloud::now_unix_ms();
    let view = RemoteCommandJobView {
        job_id: job_id.clone(),
        host: requested_host.clone(),
        state: if requested_host == "auto" {
            RemoteCommandState::Acquiring
        } else if snapshot.is_some() {
            RemoteCommandState::Preparing
        } else {
            RemoteCommandState::Queued
        },
        program: spec.argv[0].clone(),
        arg_count: spec.argv.len().saturating_sub(1),
        cwd: spec.cwd.clone(),
        expected_revision: spec.expected_revision.clone(),
        require_clean: spec.require_clean,
        timeout_s: spec.timeout_s,
        source: source_mode,
        cache: spec.cache,
        snapshot_digest: snapshot.as_ref().map(|snapshot| snapshot.digest.clone()),
        snapshot_bytes: snapshot
            .as_ref()
            .map(source::HomeSourceSnapshot::archive_bytes),
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
        result: None,
        error: None,
    };
    insert_home_job(view.clone(), caller.owner_session_id())?;

    tokio::spawn(async move {
        if let Err(error) = prepare_and_dispatch(
            job_id.clone(),
            requested_host,
            spec,
            cache_store,
            snapshot,
            branch_hint,
        )
        .await
        {
            update_home_job(&job_id, |job| {
                if !job.state.is_terminal() {
                    job.state = RemoteCommandState::Failed;
                    job.error = Some(error);
                }
            });
        }
    });

    Ok(view)
}

async fn prepare_and_dispatch(
    job_id: String,
    requested_host: String,
    mut spec: RemoteCommandSpec,
    cache_store: Option<cache_relay::HomeCacheStore>,
    snapshot: Option<source::HomeSourceSnapshot>,
    branch_hint: Option<String>,
) -> Result<(), String> {
    let host = if requested_host == "auto" {
        let acquired = scheduler::acquire_worker(&spec.expected_revision, branch_hint).await?;
        acquired.host
    } else {
        requested_host
    };
    let worker_use = scheduler::WorkerUse::begin(&host)?;
    if !home_job_is_active(&job_id) {
        return Ok(());
    }
    update_home_job(&job_id, |job| {
        job.host = host.clone();
        job.state = if snapshot.is_some() {
            RemoteCommandState::Preparing
        } else {
            RemoteCommandState::Queued
        };
    });

    if let Some(snapshot) = snapshot.as_ref() {
        spec.source_id = Some(source::transfer_snapshot(&host, snapshot).await?);
    }
    if !home_job_is_active(&job_id) {
        return Ok(());
    }

    let task_id = crate::codex_cloud_attach::cloud_host_task_id(&host)
        .ok_or_else(|| "remote job host became invalid".to_string())?
        .to_string();
    let (to_worker, from_worker) = crate::codex_cloud_attach::attachment_channel(&task_id)
        .ok_or_else(|| format!("remote host {host} has no live attachment"))?;
    let cache_session = match (cache_store, spec.cache_relay_token.as_deref()) {
        (Some(store), Some(token)) => Some(cache_relay::HomeCacheSession::register(
            &task_id, &job_id, token, store,
        )?),
        (None, None) => None,
        _ => return Err("durable cache relay preparation was internally inconsistent".into()),
    };
    let watchdog_s = spec.timeout_s.saturating_add(WORKER_WATCHDOG_GRACE_S);
    let frame = serde_json::json!({
        "t": REMOTE_COMMAND_START_KIND,
        "host_id": host,
        "id": job_id,
        "command": &spec,
    });
    if send_home_frame(&to_worker, frame.to_string())
        .await
        .is_err()
    {
        return Err(format!(
            "remote host {host} detached or stopped accepting commands before the command started"
        ));
    }
    if !home_job_is_active(&job_id) {
        let _ = send_home_frame(
            &to_worker,
            serde_json::json!({
                "t": REMOTE_COMMAND_CANCEL_KIND,
                "host_id": host,
                "id": job_id,
            })
            .to_string(),
        )
        .await;
    } else {
        update_home_job(&job_id, |job| {
            job.state = RemoteCommandState::Running;
        });
    }

    tokio::spawn(async move {
        await_worker_result(
            job_id,
            task_id,
            worker_use,
            from_worker,
            to_worker,
            cache_session,
            Duration::from_secs(watchdog_s),
        )
        .await;
    });
    Ok(())
}

fn home_job_is_active(job_id: &str) -> bool {
    home_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .jobs
        .get(job_id)
        .is_some_and(|job| !job.view.state.is_terminal())
}

async fn await_worker_result(
    job_id: String,
    task_id: String,
    _worker_use: scheduler::WorkerUse,
    mut from_worker: tokio::sync::broadcast::Receiver<String>,
    to_worker: mpsc::Sender<String>,
    mut cache_session: Option<cache_relay::HomeCacheSession>,
    watchdog: Duration,
) {
    let deadline = tokio::time::Instant::now() + watchdog;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::select! {
            worker = from_worker.recv() => match worker {
            Ok(text) => {
                let Ok(frame) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                if frame.get("t").and_then(serde_json::Value::as_str)
                    != Some(REMOTE_COMMAND_RESULT_KIND)
                    || frame.get("id").and_then(serde_json::Value::as_str) != Some(job_id.as_str())
                {
                    continue;
                }
                match frame
                    .get("result")
                    .cloned()
                    .ok_or_else(|| "worker reply carried no result".to_string())
                    .and_then(|value| {
                        serde_json::from_value::<RemoteCommandResult>(value)
                            .map_err(|error| format!("invalid worker result: {error}"))
                    })
                    .and_then(|result| {
                        if result.state.is_terminal() {
                            Ok(result)
                        } else {
                            Err(format!(
                                "worker returned non-terminal result state {:?}",
                                result.state
                            ))
                        }
                    }) {
                    Ok(result) => {
                        let state = result.state;
                        update_home_job(&job_id, |job| {
                            job.state = state;
                            job.error = result.error.clone();
                            job.result = Some(result);
                        });
                    }
                    Err(error) => {
                        update_home_job(&job_id, |job| {
                            job.state = RemoteCommandState::Failed;
                            job.error = Some(error);
                        });
                    }
                }
                return;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                update_home_job(&job_id, |job| {
                    job.state = RemoteCommandState::Failed;
                    job.error = Some("remote host detached while the command was running".into());
                });
                return;
            }
            },
            cache_frame = async {
                match cache_session.as_mut() {
                    Some(session) => session.next_frame().await,
                    None => std::future::pending().await,
                }
            } => {
                if let (Some(session), Some(frame)) = (cache_session.as_mut(), cache_frame) {
                    session.handle_frame(frame, &to_worker).await;
                }
            }
            _ = tokio::time::sleep(remaining) => break,
        }
    }

    let _ = send_home_frame(
        &to_worker,
        serde_json::json!({
            "t": REMOTE_COMMAND_CANCEL_KIND,
            "host_id": format!("{}{}", crate::codex_cloud_attach::CLOUD_HOST_PREFIX, task_id),
            "id": job_id,
        })
        .to_string(),
    )
    .await;
    update_home_job(&job_id, |job| {
        job.state = RemoteCommandState::Failed;
        job.error =
            Some("remote worker did not return a result before the watchdog expired".into());
    });
}

pub(crate) fn remote_command_status(
    job_id: &str,
    caller: &RemoteCommandCaller,
) -> Result<RemoteCommandJobView, String> {
    read_home_job(job_id, caller)
}

pub(crate) async fn wait_remote_command(
    job_id: &str,
    wait: Duration,
    caller: &RemoteCommandCaller,
) -> Result<RemoteCommandJobView, String> {
    let mut updates = subscribe_home_job(job_id, caller)?;
    if updates.borrow().state.is_terminal() {
        return Ok(updates.borrow().clone());
    }
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(updates.borrow().clone());
        }
        match tokio::time::timeout(remaining, updates.changed()).await {
            Ok(Ok(())) if updates.borrow().state.is_terminal() => {
                return Ok(updates.borrow().clone())
            }
            Ok(Ok(())) => continue,
            Ok(Err(_)) | Err(_) => return Ok(updates.borrow().clone()),
        }
    }
}

pub(crate) async fn cancel_remote_command(
    job_id: &str,
    caller: &RemoteCommandCaller,
) -> Result<RemoteCommandJobView, String> {
    let current = read_home_job(job_id, caller)?;
    if current.state.is_terminal() {
        return Ok(current);
    }
    if matches!(
        current.state,
        RemoteCommandState::Acquiring | RemoteCommandState::Preparing | RemoteCommandState::Queued
    ) {
        return update_home_job(job_id, |job| {
            if !job.state.is_terminal() {
                job.state = RemoteCommandState::Cancelled;
                job.error = Some("command was cancelled before remote execution".into());
            }
        })
        .ok_or_else(|| "remote command job was not found".to_string());
    }
    let task_id = crate::codex_cloud_attach::cloud_host_task_id(&current.host)
        .ok_or_else(|| "remote job host is no longer valid".to_string())?;
    let Some((to_worker, _)) = crate::codex_cloud_attach::attachment_channel(task_id) else {
        return update_home_job(job_id, |job| {
            if !job.state.is_terminal() {
                job.state = RemoteCommandState::Failed;
                job.error = Some("remote host detached before cancellation".into());
            }
        })
        .ok_or_else(|| "remote command job was not found".to_string());
    };
    let cancelling = update_home_job(job_id, |job| {
        if !job.state.is_terminal() {
            job.state = RemoteCommandState::Cancelling;
        }
    })
    .ok_or_else(|| "remote command job was not found".to_string())?;
    if cancelling.state.is_terminal() {
        return Ok(cancelling);
    }
    if send_home_frame(
        &to_worker,
        serde_json::json!({
            "t": REMOTE_COMMAND_CANCEL_KIND,
            "host_id": current.host,
            "id": job_id,
        })
        .to_string(),
    )
    .await
    .is_err()
    {
        return update_home_job(job_id, |job| {
            if !job.state.is_terminal() {
                job.state = RemoteCommandState::Failed;
                job.error = Some("remote host detached before cancellation".into());
            }
        })
        .ok_or_else(|| "remote command job was not found".to_string());
    }
    read_home_job(job_id, caller)
}

async fn send_home_frame(sender: &mpsc::Sender<String>, frame: String) -> Result<(), ()> {
    tokio::time::timeout(
        Duration::from_secs(WORKER_SEND_TIMEOUT_S),
        sender.send(frame),
    )
    .await
    .map_err(|_| ())?
    .map_err(|_| ())
}

pub(crate) async fn execute_remote_command_operation(
    params: crate::mcp::RemoteCommandParams,
    caller: RemoteCommandCaller,
    project_root: Option<&Path>,
) -> Result<RemoteCommandJobView, String> {
    match params {
        crate::mcp::RemoteCommandParams::Start {
            host,
            argv,
            cwd,
            env,
            source,
            expected_revision,
            require_clean,
            cache,
            timeout_s,
        } => {
            let (expected_revision, snapshot, branch_hint) = match source {
                RemoteSourceMode::GitRevision => {
                    let revision = expected_revision
                        .map(|revision| revision.trim().to_string())
                        .filter(|revision| !revision.is_empty())
                        .ok_or("expected_revision is required when source is git_revision")?;
                    let branch_hint =
                        project_root.and_then(|root| source::branch_for_revision(root, &revision));
                    (revision, None, branch_hint)
                }
                RemoteSourceMode::WorkingTree => {
                    let project_root = project_root.ok_or(
                        "working_tree source requires a supervised session with a recorded project root",
                    )?;
                    let root = project_root.to_path_buf();
                    let requested = expected_revision.clone();
                    let snapshot = tokio::task::spawn_blocking(move || {
                        source::capture_working_tree(&root, requested.as_deref())
                    })
                    .await
                    .map_err(|error| format!("working-tree capture task failed: {error}"))??;
                    let base_revision = snapshot.base_revision.clone();
                    let branch_hint = snapshot.branch_hint.clone();
                    (base_revision, Some(snapshot), branch_hint)
                }
            };
            let DurableSccacheConfig {
                cache_env,
                cache_relay_token,
                cache_store,
            } = match cache {
                RemoteCacheMode::None => DurableSccacheConfig::default(),
                RemoteCacheMode::DurableSccache => durable_sccache_config(project_root)?,
            };
            let spec = RemoteCommandSpec {
                argv,
                cwd,
                env,
                expected_revision,
                source_id: None,
                cache,
                cache_env,
                cache_relay_token,
                require_clean: require_clean.unwrap_or(true),
                timeout_s: timeout_s.unwrap_or(900),
            };
            start_remote_command(
                host,
                spec,
                cache_store,
                source,
                snapshot,
                branch_hint,
                caller,
            )
            .await
        }
        crate::mcp::RemoteCommandParams::Status { job_id } => {
            remote_command_status(&job_id, &caller)
        }
        crate::mcp::RemoteCommandParams::Wait { job_id, wait_s } => {
            let wait_s = wait_s.unwrap_or(30);
            if !(1..=60).contains(&wait_s) {
                Err("wait_s must be between 1 and 60".to_string())
            } else {
                wait_remote_command(&job_id, Duration::from_secs(wait_s), &caller).await
            }
        }
        crate::mcp::RemoteCommandParams::Cancel { job_id } => {
            cancel_remote_command(&job_id, &caller).await
        }
    }
}

#[derive(Default)]
struct DurableSccacheConfig {
    cache_env: BTreeMap<String, String>,
    cache_relay_token: Option<String>,
    cache_store: Option<cache_relay::HomeCacheStore>,
}

fn durable_sccache_config(project_root: Option<&Path>) -> Result<DurableSccacheConfig, String> {
    const TRANSPORT_ENV: &str = "INTENDANT_REMOTE_CACHE_TRANSPORT";
    let transport = std::env::var(TRANSPORT_ENV)
        .unwrap_or_else(|_| "home".to_string())
        .trim()
        .to_ascii_lowercase();
    match transport.as_str() {
        "home" | "attachment" => {
            let project_root = project_root
                .ok_or("durable_sccache through home requires a supervised project root")?;
            let store = cache_relay::HomeCacheStore::for_project(project_root)?;
            Ok(DurableSccacheConfig {
                cache_env: BTreeMap::new(),
                cache_relay_token: Some(uuid::Uuid::new_v4().simple().to_string()),
                cache_store: Some(store),
            })
        }
        "direct" => Ok(DurableSccacheConfig {
            cache_env: durable_sccache_env()?,
            cache_relay_token: None,
            cache_store: None,
        }),
        _ => Err(format!("{TRANSPORT_ENV} must be home (default) or direct")),
    }
}

fn durable_sccache_env() -> Result<BTreeMap<String, String>, String> {
    const INTENDANT_PREFIX: &str = "INTENDANT_REMOTE_CACHE_";
    const BACKEND_KEYS: &[&str] = &[
        "SCCACHE_BUCKET",
        "SCCACHE_REDIS",
        "SCCACHE_REDIS_ENDPOINT",
        "SCCACHE_REDIS_CLUSTER_ENDPOINTS",
        "SCCACHE_MEMCACHED",
        "SCCACHE_MEMCACHED_ENDPOINT",
        "SCCACHE_WEBDAV_ENDPOINT",
        "SCCACHE_GCS_BUCKET",
        "SCCACHE_AZURE_BLOB_CONTAINER",
        "SCCACHE_GHA_CACHE_URL",
        "SCCACHE_OSS_BUCKET",
        "SCCACHE_COS_BUCKET",
    ];
    let mut mapped = BTreeMap::new();
    for (key, value) in std::env::vars() {
        let Some(target) = key.strip_prefix(INTENDANT_PREFIX) else {
            continue;
        };
        if !dedicated_cache_env_key(target) {
            continue;
        }
        if managed_cache_env_key(target) {
            return Err(format!(
                "durable_sccache manages {target} itself; remove INTENDANT_REMOTE_CACHE_{target}"
            ));
        }
        if !valid_env_key(target) || value.contains('\0') {
            return Err(format!("invalid dedicated remote cache setting {key}"));
        }
        mapped.insert(target.to_string(), value);
    }
    if !BACKEND_KEYS.iter().any(|key| mapped.contains_key(*key)) {
        return Err(format!(
            "durable_sccache needs a durable backend configured through {INTENDANT_PREFIX}SCCACHE_* (for example INTENDANT_REMOTE_CACHE_SCCACHE_BUCKET); local cache directories do not qualify"
        ));
    }
    if mapped.len() > MAX_ENV_VARS
        || mapped
            .iter()
            .map(|(key, value)| key.len().saturating_add(value.len()))
            .sum::<usize>()
            > MAX_ENV_BYTES
    {
        return Err("dedicated remote cache configuration exceeds the command limits".to_string());
    }
    Ok(mapped)
}

type WorkerCancelMap = Arc<tokio::sync::Mutex<HashMap<String, Option<oneshot::Sender<()>>>>>;

#[derive(Clone)]
pub(crate) struct WorkerRemoteCommands {
    project_root: PathBuf,
    jobs: WorkerCancelMap,
    sources: source::WorkerSources,
    cache_relay: cache_relay::WorkerCacheRelay,
    retired: tokio_util::sync::CancellationToken,
}

impl WorkerRemoteCommands {
    pub(crate) fn new(project_root: PathBuf) -> Result<Self, String> {
        let project_root = project_root
            .canonicalize()
            .map_err(|error| format!("resolve worker repository root: {error}"))?;
        let sources = source::WorkerSources::new(project_root.clone())?;
        Ok(Self {
            project_root,
            jobs: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            sources,
            cache_relay: cache_relay::WorkerCacheRelay::default(),
            retired: tokio_util::sync::CancellationToken::new(),
        })
    }

    pub(crate) async fn serve_frame(
        &self,
        frame: &serde_json::Value,
        out_tx: &mpsc::Sender<String>,
        host_id: &str,
    ) -> bool {
        if self.cache_relay.serve_frame(frame).await {
            return true;
        }
        if self.sources.serve_frame(frame, out_tx, host_id).await {
            return true;
        }
        if scheduler::is_retire_frame(frame) {
            self.cancel_all().await;
            self.retired.cancel();
            return true;
        }
        match frame.get("t").and_then(serde_json::Value::as_str) {
            Some(REMOTE_COMMAND_START_KIND) => {
                self.start_from_frame(frame, out_tx, host_id).await;
                true
            }
            Some(REMOTE_COMMAND_CANCEL_KIND) => {
                let id = frame
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if let Some(sender) = self.jobs.lock().await.get_mut(id).and_then(Option::take) {
                    let _ = sender.send(());
                }
                true
            }
            _ => false,
        }
    }

    async fn start_from_frame(
        &self,
        frame: &serde_json::Value,
        out_tx: &mpsc::Sender<String>,
        host_id: &str,
    ) {
        let id = frame
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let started = Instant::now();
        if id.is_empty() || id.len() > 128 {
            send_worker_result(
                out_tx,
                host_id,
                &id,
                RemoteCommandResult::failed("remote command id is missing or too long", started),
            )
            .await;
            return;
        }
        let spec = frame
            .get("command")
            .cloned()
            .ok_or_else(|| "remote command frame carried no command".to_string())
            .and_then(|value| {
                serde_json::from_value::<RemoteCommandSpec>(value)
                    .map_err(|error| format!("invalid remote command: {error}"))
            })
            .and_then(|spec| {
                spec.validate()?;
                Ok(spec)
            });
        let spec = match spec {
            Ok(spec) => spec,
            Err(error) => {
                send_worker_result(
                    out_tx,
                    host_id,
                    &id,
                    RemoteCommandResult::failed(error, started),
                )
                .await;
                return;
            }
        };
        let (cancel_tx, mut cancel_rx) = oneshot::channel();
        {
            let mut jobs = self.jobs.lock().await;
            if jobs.contains_key(&id) {
                drop(jobs);
                send_worker_result(
                    out_tx,
                    host_id,
                    &id,
                    RemoteCommandResult::failed("remote command id is already running", started),
                )
                .await;
                return;
            }
            if jobs.len() >= WORKER_JOB_CAP {
                drop(jobs);
                send_worker_result(
                    out_tx,
                    host_id,
                    &id,
                    RemoteCommandResult::failed(
                        format!("remote worker already has {WORKER_JOB_CAP} active command jobs"),
                        started,
                    ),
                )
                .await;
                return;
            }
            jobs.insert(id.clone(), Some(cancel_tx));
        }

        let project_root = self.project_root.clone();
        let sources = self.sources.clone();
        let cache_relay = self.cache_relay.clone();
        let jobs = Arc::clone(&self.jobs);
        let reply_tx = out_tx.clone();
        let reply_host = host_id.to_string();
        tokio::spawn(async move {
            let cache_client = spec.cache_relay_token.as_ref().map(|token| {
                cache_relay.client(
                    id.clone(),
                    token.clone(),
                    reply_host.clone(),
                    reply_tx.clone(),
                )
            });
            let source_lease = match spec.source_id.as_deref() {
                Some(source_id) => {
                    let acquired = tokio::select! {
                        acquired = sources.acquire(source_id) => acquired,
                        _ = &mut cancel_rx => {
                            send_worker_result(
                                &reply_tx,
                                &reply_host,
                                &id,
                                RemoteCommandResult::cancelled(
                                    "command was cancelled while waiting for its prepared source",
                                    started,
                                ),
                            )
                            .await;
                            jobs.lock().await.remove(&id);
                            return;
                        }
                    };
                    match acquired {
                        Ok(lease) => Some(lease),
                        Err(error) => {
                            send_worker_result(
                                &reply_tx,
                                &reply_host,
                                &id,
                                RemoteCommandResult::failed(error, started),
                            )
                            .await;
                            jobs.lock().await.remove(&id);
                            return;
                        }
                    }
                }
                None => None,
            };
            let result =
                run_worker_command(&project_root, spec, source_lease, cache_client, cancel_rx)
                    .await;
            send_worker_result(&reply_tx, &reply_host, &id, result).await;
            cache_relay.cancel_job(&id).await;
            jobs.lock().await.remove(&id);
        });
    }

    pub(crate) async fn cancel_all(&self) {
        let mut jobs = self.jobs.lock().await;
        for sender in jobs.values_mut().filter_map(Option::take) {
            let _ = sender.send(());
        }
    }

    pub(crate) async fn retired(&self) {
        self.retired.cancelled().await;
    }
}

async fn send_worker_result(
    out_tx: &mpsc::Sender<String>,
    host_id: &str,
    id: &str,
    result: RemoteCommandResult,
) {
    let _ = out_tx
        .send(
            serde_json::json!({
                "t": REMOTE_COMMAND_RESULT_KIND,
                "host_id": host_id,
                "id": id,
                "result": result,
            })
            .to_string(),
        )
        .await;
}

async fn run_worker_command(
    project_root: &Path,
    spec: RemoteCommandSpec,
    source_lease: Option<source::WorkerSourceLease>,
    cache_client: Option<cache_relay::WorkerCacheClient>,
    mut cancel_rx: oneshot::Receiver<()>,
) -> RemoteCommandResult {
    let started = Instant::now();
    let selected_root = source_lease
        .as_ref()
        .map(|lease| lease.root.as_path())
        .unwrap_or(project_root);
    let baseline_source_digest = source_lease
        .as_ref()
        .map(|lease| lease.baseline_source_digest.as_str());
    let revision = match git_text(selected_root, &["rev-parse", "HEAD"]).await {
        Ok(revision) => revision.trim().to_ascii_lowercase(),
        Err(error) => return RemoteCommandResult::failed(error, started),
    };
    let expected = spec.expected_revision.trim().to_ascii_lowercase();
    if !revision.starts_with(&expected) {
        return RemoteCommandResult::failed(
            format!(
                "worker revision mismatch: expected {expected}, worker has {revision}; push/select the intended revision before remote validation"
            ),
            started,
        );
    }
    if spec.require_clean {
        let clean = match baseline_source_digest {
            Some(expected) => source::workspace_source_digest(selected_root)
                .await
                .map(|actual| actual == expected),
            None => workspace_status(selected_root)
                .await
                .map(|status| status.is_empty()),
        };
        match clean {
            Ok(true) => {}
            Ok(false) => {
                return RemoteCommandResult::failed(
                    "worker source differs from the selected checkout/snapshot before execution",
                    started,
                )
            }
            Err(error) => return RemoteCommandResult::failed(error, started),
        }
    }
    let cwd = match resolve_worker_cwd(selected_root, spec.cwd.as_deref()) {
        Ok(cwd) => cwd,
        Err(error) => return RemoteCommandResult::failed(error, started),
    };
    // sccache exposes server-wide counters, not per-command counters. Hold a
    // cache-config + source-root lane through baseline → command → final
    // stats so reported deltas belong to this job rather than an overlapping
    // sibling. A root-specific server also lets SCCACHE_BASEDIRS normalize
    // each ephemeral worktree before cache keys are computed.
    let _cache_guard = if spec.cache == RemoteCacheMode::DurableSccache {
        let lock = cache_lock_for(&spec.cache_env, selected_root);
        let guard = tokio::select! {
            guard = lock.lock_owned() => guard,
            _ = &mut cancel_rx => {
                return RemoteCommandResult::cancelled(
                    "command was cancelled while waiting for its durable cache lane",
                    started,
                )
            }
        };
        Some(guard)
    } else {
        None
    };
    let cache_execution = tokio::select! {
        cache = prepare_cache(&spec, selected_root, cache_client) => match cache {
            Ok(cache) => cache,
            Err(error) => return RemoteCommandResult::failed(error, started),
        },
        _ = &mut cancel_rx => {
            return RemoteCommandResult::cancelled(
                "command was cancelled while preparing its durable cache",
                started,
            )
        }
    };

    let mut command = crate::platform::spawn_command(&spec.argv[0]);
    // The attachment agent's INTENDANT_HOME contains its ephemeral client
    // identity. Remote workloads must not inherit that private control-plane
    // state. Apply the caller's explicit environment afterwards so a workload
    // that deliberately needs its own INTENDANT_HOME can still request one.
    remove_worker_private_state(&mut command);
    command
        .args(&spec.argv[1..])
        .current_dir(cwd)
        .envs(&spec.env)
        .envs(
            cache_execution
                .as_ref()
                .map(|cache| &cache.command_env)
                .into_iter()
                .flatten(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if cache_execution.is_some() {
        // Do not let either the worker environment or the requested command
        // opt into bypassing a failed sccache client/server connection.
        // Keep sccache's default strict client behavior.
        command.env_remove("SCCACHE_IGNORE_SERVER_IO_ERROR");
    }
    crate::platform::die_with_parent(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return RemoteCommandResult::failed(
                format!("spawn '{}': {error}", spec.argv[0]),
                started,
            )
        }
    };
    let pid = child.id().unwrap_or(0);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::spawn(async move { read_bounded(stdout).await });
    let stderr_task = tokio::spawn(async move { read_bounded(stderr).await });

    enum Outcome {
        Exited(std::io::Result<std::process::ExitStatus>),
        Cancelled,
        TimedOut,
    }
    let timeout = tokio::time::sleep(Duration::from_secs(spec.timeout_s));
    tokio::pin!(timeout);
    let outcome = tokio::select! {
        status = child.wait() => Outcome::Exited(status),
        _ = &mut cancel_rx => Outcome::Cancelled,
        _ = &mut timeout => Outcome::TimedOut,
    };
    if !matches!(outcome, Outcome::Exited(_)) && pid != 0 {
        let _ =
            tokio::task::spawn_blocking(move || crate::platform::terminate_process_tree_now(pid))
                .await;
        let _ = child.wait().await;
    }

    let ((stdout, stdout_truncated), (stderr, stderr_truncated)) = tokio::join!(
        finish_output_reader(stdout_task, "stdout"),
        finish_output_reader(stderr_task, "stderr"),
    );
    let (state, exit_code, error) = match outcome {
        Outcome::Exited(Ok(status)) if status.success() => {
            (RemoteCommandState::Succeeded, status.code(), None)
        }
        Outcome::Exited(Ok(status)) => (
            RemoteCommandState::Failed,
            status.code(),
            Some(format!("command exited with status {status}")),
        ),
        Outcome::Exited(Err(error)) => (
            RemoteCommandState::Failed,
            None,
            Some(format!("wait for command: {error}")),
        ),
        Outcome::Cancelled => (
            RemoteCommandState::Cancelled,
            None,
            Some("command was cancelled".to_string()),
        ),
        Outcome::TimedOut => (
            RemoteCommandState::TimedOut,
            None,
            Some(format!("command exceeded its {}s timeout", spec.timeout_s)),
        ),
    };
    let workspace_dirty_after = match baseline_source_digest {
        Some(expected) => source::workspace_source_digest(selected_root)
            .await
            .ok()
            .map(|actual| actual != expected),
        None => workspace_status(selected_root)
            .await
            .ok()
            .map(|status| !status.is_empty()),
    };
    let cache = match cache_execution {
        Some(cache) => Some(finish_cache(cache).await),
        None => None,
    };
    RemoteCommandResult {
        state,
        exit_code,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        duration_ms: elapsed_ms(started),
        error,
        worker_revision: Some(revision),
        workspace_dirty_after,
        cache,
    }
}

#[derive(Debug, Clone, Default)]
struct CacheStats {
    hits: u64,
    misses: u64,
    writes: u64,
    errors: u64,
    location: Option<String>,
}

struct CacheExecution {
    command_env: BTreeMap<String, String>,
    stats_env: BTreeMap<String, String>,
    before: CacheStats,
    /// Keeps the loopback WebDAV relay alive through the final stats read.
    sidecar: Option<cache_relay::WorkerCacheSidecar>,
    stop_server: bool,
}

type CacheLockMap = HashMap<String, std::sync::Weak<tokio::sync::Mutex<()>>>;

static CACHE_LOCKS: OnceLock<Mutex<CacheLockMap>> = OnceLock::new();

fn cache_lock_for(
    config: &BTreeMap<String, String>,
    selected_root: &Path,
) -> Arc<tokio::sync::Mutex<()>> {
    let key = cache_lane_digest(config, selected_root);
    let mut locks = CACHE_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

fn cache_config_digest(config: &BTreeMap<String, String>) -> String {
    let config_json = serde_json::to_vec(config).unwrap_or_default();
    sha2::Sha256::digest(config_json)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn cache_lane_digest(config: &BTreeMap<String, String>, selected_root: &Path) -> String {
    let mut digest = sha2::Sha256::new();
    digest.update(cache_config_digest(config).as_bytes());
    digest.update([0]);
    digest.update(selected_root.to_string_lossy().as_bytes());
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn prepare_cache(
    spec: &RemoteCommandSpec,
    selected_root: &Path,
    cache_client: Option<cache_relay::WorkerCacheClient>,
) -> Result<Option<CacheExecution>, String> {
    if spec.cache == RemoteCacheMode::None {
        return Ok(None);
    }
    let (mut stats_env, sidecar, stop_server) = match (
        spec.cache_relay_token.as_ref(),
        cache_client,
    ) {
        (Some(_), Some(client)) => {
            let sidecar = cache_relay::WorkerCacheSidecar::start(client).await?;
            let mut env = BTreeMap::new();
            env.insert(
                "SCCACHE_WEBDAV_ENDPOINT".to_string(),
                sidecar.endpoint().to_string(),
            );
            (env, Some(sidecar), true)
        }
        (None, None) if !spec.cache_env.is_empty() => (spec.cache_env.clone(), None, false),
        _ => {
            return Err(
                "durable_sccache carried no usable durable transport; refusing an implicit worker-local cache"
                    .to_string(),
            )
        }
    };
    let digest = cache_lane_digest(&stats_env, selected_root);
    let socket = std::env::temp_dir().join(format!("intendant-sccache-{}.sock", &digest[..24]));
    stats_env.insert(
        "SCCACHE_SERVER_UDS".into(),
        socket.to_string_lossy().into_owned(),
    );
    stats_env.insert(
        "SCCACHE_IDLE_TIMEOUT".into(),
        CACHE_SERVER_IDLE_TIMEOUT_S.to_string(),
    );
    stats_env.insert(
        "SCCACHE_BASEDIRS".into(),
        selected_root.display().to_string(),
    );

    let version = run_sccache(&["--version"], &stats_env)
        .await
        .map_err(|error| format!("durable_sccache requires sccache on the worker: {error}"))?;
    if !sccache_supports_basedirs(&version) {
        return Err(format!(
            "durable_sccache requires sccache 0.14 or newer; worker reported {:?}",
            version.trim()
        ));
    }
    if let Err(start_error) = run_sccache(&["--start-server"], &stats_env).await {
        // Another command may have won the same config-keyed start race. A
        // successful stats query proves the expected server is available.
        if run_sccache_stats(&stats_env).await.is_err() {
            return Err(format!(
                "start the durable sccache server for this source lane: {start_error}"
            ));
        }
    }
    let before = run_sccache_stats(&stats_env)
        .await
        .map_err(|error| format!("read durable sccache baseline stats: {error}"))?;
    if !durable_cache_location(before.location.as_deref()) {
        return Err(
            "sccache started without an external durable backend; refusing worker-local cache"
                .to_string(),
        );
    }
    // sccache clients automatically start a missing local server. Carry the
    // same dedicated backend configuration into the requested build so such
    // a restart cannot silently become the default local-disk cache. These
    // values therefore require a cache-only principal; the worker is not a
    // credential enclave.
    let mut command_env = stats_env.clone();
    command_env.insert("RUSTC_WRAPPER".into(), "sccache".into());
    command_env.insert("CARGO_INCREMENTAL".into(), "0".into());
    command_env.insert(
        "SCCACHE_BASEDIRS".into(),
        selected_root.display().to_string(),
    );
    Ok(Some(CacheExecution {
        command_env,
        stats_env,
        before,
        sidecar,
        stop_server,
    }))
}

async fn finish_cache(cache: CacheExecution) -> RemoteCacheReport {
    let report = match run_sccache_stats(&cache.stats_env).await {
        Ok(after) => RemoteCacheReport {
            mode: RemoteCacheMode::DurableSccache,
            hits_delta: after.hits.saturating_sub(cache.before.hits),
            misses_delta: after.misses.saturating_sub(cache.before.misses),
            writes_delta: after.writes.saturating_sub(cache.before.writes),
            errors_delta: after.errors.saturating_sub(cache.before.errors),
            stats_error: (!durable_cache_location(after.location.as_deref())).then(|| {
                "sccache no longer reports an external durable backend after the command"
                    .to_string()
            }),
        },
        Err(error) => RemoteCacheReport {
            mode: RemoteCacheMode::DurableSccache,
            hits_delta: 0,
            misses_delta: 0,
            writes_delta: 0,
            errors_delta: 0,
            stats_error: Some(format!("read final sccache stats: {error}")),
        },
    };
    if cache.stop_server {
        let _ = run_sccache(&["--stop-server"], &cache.stats_env).await;
    }
    drop(cache.sidecar);
    report
}

async fn run_sccache(args: &[&str], env: &BTreeMap<String, String>) -> Result<String, String> {
    let mut command = crate::platform::spawn_command("sccache");
    command
        .args(args)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .map_err(|_| format!("sccache {} timed out", args.join(" ")))?
        .map_err(|error| format!("run sccache {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        let detail = redact_cache_values(String::from_utf8_lossy(&output.stderr).trim(), env);
        return Err(format!(
            "sccache {} exited with {}: {}",
            args.join(" "),
            output.status,
            detail
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn redact_cache_values(text: &str, env: &BTreeMap<String, String>) -> String {
    env.values()
        .filter(|value| value.len() >= 4)
        .fold(text.to_string(), |redacted, value| {
            redacted.replace(value, "[redacted-cache-value]")
        })
}

async fn run_sccache_stats(env: &BTreeMap<String, String>) -> Result<CacheStats, String> {
    let text = run_sccache(&["--show-stats", "--stats-format=json"], env).await?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("parse sccache stats JSON: {error}"))?;
    let errors = [
        "cache_errors",
        "cache_read_errors",
        "cache_write_errors",
        "cache_timeouts",
        "dist_errors",
    ]
    .iter()
    .filter_map(|key| cache_metric(&value, key))
    .fold(0u64, u64::saturating_add);
    Ok(CacheStats {
        hits: cache_metric(&value, "cache_hits").unwrap_or(0),
        misses: cache_metric(&value, "cache_misses").unwrap_or(0),
        writes: cache_metric(&value, "cache_writes").unwrap_or(0),
        errors,
        location: value
            .get("cache_location")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

fn durable_cache_location(location: Option<&str>) -> bool {
    location.is_some_and(|location| !location.trim_start().starts_with("Local disk:"))
}

fn sccache_supports_basedirs(version: &str) -> bool {
    version
        .split_whitespace()
        .find_map(|word| {
            let word = word.trim_start_matches('v');
            let mut parts = word.split('.');
            let major = parts.next()?.parse::<u64>().ok()?;
            let minor = parts.next()?.parse::<u64>().ok()?;
            Some(major > 0 || minor >= 14)
        })
        .unwrap_or(false)
}

fn cache_metric(value: &serde_json::Value, key: &str) -> Option<u64> {
    match value {
        serde_json::Value::Object(map) => map
            .get(key)
            .map(sum_cache_metric)
            .or_else(|| map.values().find_map(|value| cache_metric(value, key))),
        serde_json::Value::Array(values) => {
            values.iter().find_map(|value| cache_metric(value, key))
        }
        _ => None,
    }
}

fn sum_cache_metric(value: &serde_json::Value) -> u64 {
    // Current sccache JSON carries the same totals in the human-facing
    // `counts` and normalized `adv_counts` maps. Prefer `counts` instead of
    // adding both representations together.
    match value {
        serde_json::Value::Object(map) if map.contains_key("counts") => {
            map.get("counts").map(sum_json_numbers).unwrap_or(0)
        }
        _ => sum_json_numbers(value),
    }
}

fn sum_json_numbers(value: &serde_json::Value) -> u64 {
    match value {
        serde_json::Value::Number(number) => number.as_u64().unwrap_or(0),
        serde_json::Value::Object(map) => map.values().fold(0u64, |sum, value| {
            sum.saturating_add(sum_json_numbers(value))
        }),
        serde_json::Value::Array(values) => values.iter().fold(0u64, |sum, value| {
            sum.saturating_add(sum_json_numbers(value))
        }),
        _ => 0,
    }
}

async fn finish_output_reader(
    mut task: tokio::task::JoinHandle<(String, bool)>,
    stream: &str,
) -> (String, bool) {
    match tokio::time::timeout(Duration::from_secs(OUTPUT_DRAIN_TIMEOUT_S), &mut task).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => (format!("[{stream} reader failed: {error}]"), false),
        Err(_) => {
            task.abort();
            (
                format!(
                    "[{stream} did not close within {OUTPUT_DRAIN_TIMEOUT_S}s after the command exited]"
                ),
                true,
            )
        }
    }
}

async fn git_text(project_root: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = crate::platform::spawn_command("git");
    command
        .args(args)
        .current_dir(project_root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .map_err(|_| format!("git {} timed out", args.join(" ")))?
        .map_err(|error| format!("run git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {} failed: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn workspace_status(project_root: &Path) -> Result<String, String> {
    git_text(
        project_root,
        &["status", "--porcelain=v1", "--untracked-files=normal"],
    )
    .await
}

fn resolve_worker_cwd(project_root: &Path, raw: Option<&str>) -> Result<PathBuf, String> {
    let candidate = match raw {
        None | Some(".") => project_root.to_path_buf(),
        Some(raw) => {
            validate_relative_cwd(raw)?;
            project_root.join(raw)
        }
    };
    let candidate = candidate
        .canonicalize()
        .map_err(|error| format!("resolve remote cwd {}: {error}", candidate.display()))?;
    if !candidate.starts_with(project_root) {
        return Err("remote cwd resolves outside the repository".to_string());
    }
    if !candidate.is_dir() {
        return Err("remote cwd is not a directory".to_string());
    }
    Ok(candidate)
}

async fn read_bounded<R>(reader: Option<R>) -> (String, bool)
where
    R: AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return (String::new(), false);
    };
    let mut output = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) => {
                let suffix = format!("\n[output read failed: {error}]");
                retain_bounded(&mut output, suffix.as_bytes(), &mut truncated);
                break;
            }
        };
        retain_bounded(&mut output, &chunk[..read], &mut truncated);
    }
    (String::from_utf8_lossy(&output).into_owned(), truncated)
}

fn retain_bounded(output: &mut Vec<u8>, incoming: &[u8], truncated: &mut bool) {
    if !*truncated && output.len().saturating_add(incoming.len()) <= OUTPUT_LIMIT_BYTES {
        output.extend_from_slice(incoming);
        return;
    }
    let tail_limit = OUTPUT_LIMIT_BYTES.saturating_sub(OUTPUT_HEAD_BYTES);
    let mut tail = if output.len() > OUTPUT_HEAD_BYTES {
        output[OUTPUT_HEAD_BYTES..].to_vec()
    } else {
        Vec::new()
    };
    tail.extend_from_slice(incoming);
    if tail.len() > tail_limit {
        let drop = tail.len() - tail_limit;
        tail.drain(..drop);
    }
    output.truncate(OUTPUT_HEAD_BYTES.min(output.len()));
    output.extend_from_slice(&tail);
    *truncated = true;
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("run git {}: {error}", args.join(" ")));
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn clean_git_repo() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet"]);
        std::fs::write(dir.path().join("README.md"), "remote compute fixture\n").unwrap();
        git(dir.path(), &["add", "README.md"]);
        git(
            dir.path(),
            &[
                "-c",
                "user.name=Intendant Test",
                "-c",
                "user.email=intendant-test@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let revision = git(dir.path(), &["rev-parse", "HEAD"]);
        (dir, revision)
    }

    fn spec() -> RemoteCommandSpec {
        RemoteCommandSpec {
            argv: vec!["cargo".into(), "check".into()],
            cwd: Some("crates/intendant-core".into()),
            env: BTreeMap::from([("CARGO_INCREMENTAL".into(), "0".into())]),
            expected_revision: "0123456789abcdef".into(),
            source_id: None,
            cache: RemoteCacheMode::None,
            cache_env: BTreeMap::new(),
            cache_relay_token: None,
            require_clean: true,
            timeout_s: 300,
        }
    }

    #[test]
    fn command_spec_is_argv_revision_and_repo_relative() {
        spec().validate().unwrap();

        let mut shell = spec();
        shell.argv.clear();
        assert!(shell.validate().unwrap_err().contains("executable"));

        let mut traversal = spec();
        traversal.cwd = Some("../other".into());
        assert!(traversal.validate().unwrap_err().contains("parent"));

        let mut branch = spec();
        branch.expected_revision = "main".into();
        assert!(branch
            .validate()
            .unwrap_err()
            .contains("hexadecimal Git object id"));

        let mut bad_env = spec();
        bad_env.env = BTreeMap::from([("NOT-AN-ENV".into(), "x".into())]);
        assert!(bad_env
            .validate()
            .unwrap_err()
            .contains("invalid environment variable"));
    }

    #[test]
    fn bounded_output_keeps_the_head_and_latest_tail() {
        let mut output = vec![b'h'; OUTPUT_HEAD_BYTES];
        let middle = vec![b'm'; OUTPUT_LIMIT_BYTES];
        let tail = vec![b't'; 4096];
        let mut truncated = false;
        retain_bounded(&mut output, &middle, &mut truncated);
        retain_bounded(&mut output, &tail, &mut truncated);
        assert!(truncated);
        assert_eq!(output.len(), OUTPUT_LIMIT_BYTES);
        assert!(output[..OUTPUT_HEAD_BYTES].iter().all(|byte| *byte == b'h'));
        assert!(output[OUTPUT_LIMIT_BYTES - tail.len()..]
            .iter()
            .all(|byte| *byte == b't'));
    }

    #[test]
    fn cache_stats_are_summed_without_exposing_cache_values_in_debug() {
        let stats = serde_json::json!({
            "stats": {
                "cache_hits": {
                    "counts": {"Rust": 7, "C/C++": 2},
                    "adv_counts": {"rust": 7, "c_cpp": 2}
                },
                "cache_misses": {
                    "counts": {"Rust": 3},
                    "adv_counts": {"rust": 3}
                },
                "cache_writes": 4,
                "cache_errors": {"timeout": 1},
                "cache_write_errors": 2
            }
        });
        assert_eq!(cache_metric(&stats, "cache_hits"), Some(9));
        assert_eq!(cache_metric(&stats, "cache_misses"), Some(3));
        assert_eq!(cache_metric(&stats, "cache_writes"), Some(4));
        assert_eq!(cache_metric(&stats, "cache_errors"), Some(1));
        assert_eq!(cache_metric(&stats, "cache_write_errors"), Some(2));
        assert!(!durable_cache_location(Some("Local disk: \"/tmp/cache\"")));
        assert!(durable_cache_location(Some("Redis: redis://cache")));
        assert!(!durable_cache_location(None));
        assert!(!sccache_supports_basedirs("sccache 0.13.0"));
        assert!(sccache_supports_basedirs("sccache 0.14.0"));
        assert!(sccache_supports_basedirs("sccache v1.0.0"));

        let mut command = spec();
        command.cache = RemoteCacheMode::DurableSccache;
        command.cache_env = BTreeMap::from([("SCCACHE_BUCKET".into(), "secret-bucket".into())]);
        let debug = format!("{command:?}");
        assert!(debug.contains("SCCACHE_BUCKET"));
        assert!(!debug.contains("secret-bucket"));
        assert_eq!(
            redact_cache_values("backend secret-bucket refused", &command.cache_env,),
            "backend [redacted-cache-value] refused"
        );
        assert_ne!(
            cache_lane_digest(&command.cache_env, Path::new("source-a")),
            cache_lane_digest(&command.cache_env, Path::new("source-b"))
        );

        command.cache_env = BTreeMap::from([
            ("ALIBABA_CLOUD_ACCESS_KEY_ID".into(), "cache-only-id".into()),
            ("SCCACHE_OSS_BUCKET".into(), "compiler-cache".into()),
        ]);
        command.validate().unwrap();
        command.cache_env = BTreeMap::from([("PATH".into(), "/not/dedicated".into())]);
        assert!(command
            .validate()
            .unwrap_err()
            .contains("invalid dedicated cache"));
        command.cache_env = BTreeMap::from([("SCCACHE_DIR".into(), "/tmp/local".into())]);
        assert!(command
            .validate()
            .unwrap_err()
            .contains("invalid dedicated cache"));

        command.cache_env.clear();
        command.cache_relay_token = Some("a".repeat(32));
        command.validate().unwrap();
        assert!(!format!("{command:?}").contains(&"a".repeat(32)));
        command.cache_env = BTreeMap::from([("SCCACHE_BUCKET".into(), "compiler-cache".into())]);
        assert!(command
            .validate()
            .unwrap_err()
            .contains("exactly one durable cache transport"));
    }

    #[test]
    fn remote_command_does_not_inherit_worker_private_state() {
        let mut command = tokio::process::Command::new("unused");
        remove_worker_private_state(&mut command);
        assert_eq!(
            command
                .as_std()
                .get_envs()
                .find(|(name, _)| *name == WORKER_PRIVATE_STATE_ENV)
                .map(|(_, value)| value),
            Some(None)
        );

        command.env(WORKER_PRIVATE_STATE_ENV, "/caller-selected-state");
        assert_eq!(
            command
                .as_std()
                .get_envs()
                .find(|(name, _)| *name == WORKER_PRIVATE_STATE_ENV)
                .and_then(|(_, value)| value),
            Some(std::ffi::OsStr::new("/caller-selected-state"))
        );
    }

    #[test]
    fn session_owned_jobs_are_not_cross_session_visible() {
        let owner = Some("session-a");
        assert!(RemoteCommandCaller::AgentSession("session-a".into()).may_access(owner));
        assert!(!RemoteCommandCaller::AgentSession("session-b".into()).may_access(owner));
        assert!(RemoteCommandCaller::Unrestricted.may_access(owner));
        assert!(!RemoteCommandCaller::AgentSession("session-a".into()).may_access(None));
    }

    #[tokio::test]
    async fn wait_sees_a_terminal_update_that_preceded_subscription() {
        let job_id = format!("remote-watch-test-{}", uuid::Uuid::new_v4());
        let now = crate::codex_cloud::now_unix_ms();
        insert_home_job(
            RemoteCommandJobView {
                job_id: job_id.clone(),
                host: "cloud:test-task".into(),
                state: RemoteCommandState::Queued,
                program: "git".into(),
                arg_count: 0,
                cwd: None,
                expected_revision: "0123456".into(),
                require_clean: true,
                timeout_s: 10,
                source: RemoteSourceMode::GitRevision,
                cache: RemoteCacheMode::None,
                snapshot_digest: None,
                snapshot_bytes: None,
                created_at_unix_ms: now,
                updated_at_unix_ms: now,
                result: None,
                error: None,
            },
            None,
        )
        .unwrap();
        update_home_job(&job_id, |job| {
            job.state = RemoteCommandState::Succeeded;
        })
        .unwrap();

        let result = wait_remote_command(
            &job_id,
            Duration::from_millis(10),
            &RemoteCommandCaller::Unrestricted,
        )
        .await
        .unwrap();
        remove_home_job(&job_id);
        assert_eq!(result.state, RemoteCommandState::Succeeded);
    }

    #[tokio::test]
    async fn worker_refuses_revision_drift_and_dirty_source() {
        let (repo, revision) = clean_git_repo();
        // Production enters through `WorkerRemoteCommands::new`, which
        // canonicalizes the repository root. Mirror that invariant here:
        // macOS temp paths commonly traverse /var -> /private/var.
        let project_root = repo.path().canonicalize().unwrap();
        let command = RemoteCommandSpec {
            argv: vec!["git".into(), "rev-parse".into(), "HEAD".into()],
            cwd: None,
            env: BTreeMap::new(),
            expected_revision: revision.clone(),
            source_id: None,
            cache: RemoteCacheMode::None,
            cache_env: BTreeMap::new(),
            cache_relay_token: None,
            require_clean: true,
            timeout_s: 10,
        };

        let (cancel_tx, cancel_rx) = oneshot::channel();
        let result =
            run_worker_command(&project_root, command.clone(), None, None, cancel_rx).await;
        drop(cancel_tx);
        assert_eq!(result.state, RemoteCommandState::Succeeded, "{result:#?}");
        assert_eq!(result.stdout.trim(), revision);
        assert_eq!(result.worker_revision.as_deref(), Some(revision.as_str()));
        assert_eq!(result.workspace_dirty_after, Some(false));

        let mut wrong_revision = command.clone();
        wrong_revision.expected_revision = "deadbee".into();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let result = run_worker_command(&project_root, wrong_revision, None, None, cancel_rx).await;
        drop(cancel_tx);
        assert_eq!(result.state, RemoteCommandState::Failed);
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("revision mismatch")));

        std::fs::write(repo.path().join("uncommitted.txt"), "not selected\n").unwrap();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let result = run_worker_command(&project_root, command, None, None, cancel_rx).await;
        drop(cancel_tx);
        assert_eq!(result.state, RemoteCommandState::Failed);
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("differs from the selected")));
    }

    #[tokio::test]
    async fn worker_frame_executes_and_reports_a_terminal_result() {
        let (repo, revision) = clean_git_repo();
        let worker = WorkerRemoteCommands::new(repo.path().to_path_buf()).unwrap();
        let (out_tx, mut out_rx) = mpsc::channel(8);
        let frame = serde_json::json!({
            "t": REMOTE_COMMAND_START_KIND,
            "host_id": "cloud:test-task",
            "id": "remote-frame-test",
            "command": {
                "argv": ["git", "rev-parse", "HEAD"],
                "expected_revision": revision.clone(),
                "require_clean": true,
                "timeout_s": 10,
            },
        });

        assert!(worker.serve_frame(&frame, &out_tx, "cloud:test-task").await);
        let text = tokio::time::timeout(Duration::from_secs(10), out_rx.recv())
            .await
            .expect("worker reply deadline")
            .expect("worker reply");
        let reply: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reply["t"], REMOTE_COMMAND_RESULT_KIND);
        assert_eq!(reply["id"], "remote-frame-test");
        let result: RemoteCommandResult = serde_json::from_value(reply["result"].clone()).unwrap();
        assert_eq!(result.state, RemoteCommandState::Succeeded);
        assert_eq!(result.stdout.trim(), revision);
    }
}
