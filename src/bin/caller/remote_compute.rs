//! Provider-neutral remote command jobs.
//!
//! The public contract is deliberately host-shaped (`cloud:<task-id>` is the
//! first backend), while the Codex Cloud attachment is only one transport.
//! Commands are argv arrays, never shell strings. A required Git revision and
//! clean-worktree default keep a caller from accidentally validating a
//! different source tree than the one it intended to send.

use serde::{Deserialize, Serialize};
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

pub(crate) const REMOTE_COMMAND_START_KIND: &str = "remote_command_start";
pub(crate) const REMOTE_COMMAND_CANCEL_KIND: &str = "remote_command_cancel";
pub(crate) const REMOTE_COMMAND_RESULT_KIND: &str = "remote_command_result";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RemoteCommandSpec {
    pub argv: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub expected_revision: String,
    #[serde(default = "default_require_clean")]
    pub require_clean: bool,
    pub timeout_s: u64,
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
        }
    }
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

pub(crate) async fn start_remote_command(
    host: &str,
    spec: RemoteCommandSpec,
    caller: RemoteCommandCaller,
) -> Result<RemoteCommandJobView, String> {
    spec.validate()?;
    let task_id = crate::codex_cloud_attach::cloud_host_task_id(host)
        .ok_or_else(|| {
            "unsupported remote host; this release accepts cloud:<codex-task-id>".to_string()
        })?
        .to_string();
    let Some((to_worker, from_worker)) = crate::codex_cloud_attach::attachment_channel(&task_id)
    else {
        return Err(format!("remote host {host} has no live attachment"));
    };

    let job_id = format!("remote-{}", uuid::Uuid::new_v4());
    let now = crate::codex_cloud::now_unix_ms();
    let view = RemoteCommandJobView {
        job_id: job_id.clone(),
        host: host.to_string(),
        state: RemoteCommandState::Queued,
        program: spec.argv[0].clone(),
        arg_count: spec.argv.len().saturating_sub(1),
        cwd: spec.cwd.clone(),
        expected_revision: spec.expected_revision.clone(),
        require_clean: spec.require_clean,
        timeout_s: spec.timeout_s,
        created_at_unix_ms: now,
        updated_at_unix_ms: now,
        result: None,
        error: None,
    };
    insert_home_job(view, caller.owner_session_id())?;

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
        remove_home_job(&job_id);
        return Err(format!(
            "remote host {host} detached or stopped accepting commands before the command started"
        ));
    }
    let view = update_home_job(&job_id, |job| {
        job.state = RemoteCommandState::Running;
    })
    .expect("job was inserted immediately above");

    let waiter_job_id = job_id.clone();
    let waiter_to_worker = to_worker.clone();
    tokio::spawn(async move {
        await_worker_result(
            waiter_job_id,
            task_id,
            from_worker,
            waiter_to_worker,
            Duration::from_secs(watchdog_s),
        )
        .await;
    });

    Ok(view)
}

async fn await_worker_result(
    job_id: String,
    task_id: String,
    mut from_worker: tokio::sync::broadcast::Receiver<String>,
    to_worker: mpsc::Sender<String>,
    watchdog: Duration,
) {
    let deadline = tokio::time::Instant::now() + watchdog;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, from_worker.recv()).await {
            Ok(Ok(text)) => {
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
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                update_home_job(&job_id, |job| {
                    job.state = RemoteCommandState::Failed;
                    job.error = Some("remote host detached while the command was running".into());
                });
                return;
            }
            Err(_) => break,
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
) -> Result<RemoteCommandJobView, String> {
    match params {
        crate::mcp::RemoteCommandParams::Start {
            host,
            argv,
            cwd,
            env,
            expected_revision,
            require_clean,
            timeout_s,
        } => {
            let spec = RemoteCommandSpec {
                argv,
                cwd,
                env,
                expected_revision,
                require_clean: require_clean.unwrap_or(true),
                timeout_s: timeout_s.unwrap_or(900),
            };
            start_remote_command(&host, spec, caller).await
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

type WorkerCancelMap = Arc<tokio::sync::Mutex<HashMap<String, Option<oneshot::Sender<()>>>>>;

#[derive(Clone)]
pub(crate) struct WorkerRemoteCommands {
    project_root: PathBuf,
    jobs: WorkerCancelMap,
}

impl WorkerRemoteCommands {
    pub(crate) fn new(project_root: PathBuf) -> Result<Self, String> {
        let project_root = project_root
            .canonicalize()
            .map_err(|error| format!("resolve worker repository root: {error}"))?;
        Ok(Self {
            project_root,
            jobs: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        })
    }

    pub(crate) async fn serve_frame(
        &self,
        frame: &serde_json::Value,
        out_tx: &mpsc::Sender<String>,
        host_id: &str,
    ) -> bool {
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
        let (cancel_tx, cancel_rx) = oneshot::channel();
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
        let jobs = Arc::clone(&self.jobs);
        let reply_tx = out_tx.clone();
        let reply_host = host_id.to_string();
        tokio::spawn(async move {
            let result = run_worker_command(&project_root, spec, cancel_rx).await;
            send_worker_result(&reply_tx, &reply_host, &id, result).await;
            jobs.lock().await.remove(&id);
        });
    }

    pub(crate) async fn cancel_all(&self) {
        let mut jobs = self.jobs.lock().await;
        for sender in jobs.values_mut().filter_map(Option::take) {
            let _ = sender.send(());
        }
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
    mut cancel_rx: oneshot::Receiver<()>,
) -> RemoteCommandResult {
    let started = Instant::now();
    let revision = match git_text(project_root, &["rev-parse", "HEAD"]).await {
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
        match workspace_status(project_root).await {
            Ok(status) if status.is_empty() => {}
            Ok(status) => {
                return RemoteCommandResult::failed(
                    format!(
                        "worker checkout is dirty before execution: {}",
                        status.lines().take(20).collect::<Vec<_>>().join("\n")
                    ),
                    started,
                )
            }
            Err(error) => return RemoteCommandResult::failed(error, started),
        }
    }
    let cwd = match resolve_worker_cwd(project_root, spec.cwd.as_deref()) {
        Ok(cwd) => cwd,
        Err(error) => return RemoteCommandResult::failed(error, started),
    };

    let mut command = crate::platform::spawn_command(&spec.argv[0]);
    command
        .args(&spec.argv[1..])
        .current_dir(cwd)
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
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
    let workspace_dirty_after = workspace_status(project_root)
        .await
        .ok()
        .map(|status| !status.is_empty());
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
            require_clean: true,
            timeout_s: 10,
        };

        let (cancel_tx, cancel_rx) = oneshot::channel();
        let result = run_worker_command(&project_root, command.clone(), cancel_rx).await;
        drop(cancel_tx);
        assert_eq!(result.state, RemoteCommandState::Succeeded, "{result:#?}");
        assert_eq!(result.stdout.trim(), revision);
        assert_eq!(result.worker_revision.as_deref(), Some(revision.as_str()));
        assert_eq!(result.workspace_dirty_after, Some(false));

        let mut wrong_revision = command.clone();
        wrong_revision.expected_revision = "deadbee".into();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let result = run_worker_command(&project_root, wrong_revision, cancel_rx).await;
        drop(cancel_tx);
        assert_eq!(result.state, RemoteCommandState::Failed);
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("revision mismatch")));

        std::fs::write(repo.path().join("uncommitted.txt"), "not selected\n").unwrap();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let result = run_worker_command(&project_root, command, cancel_rx).await;
        drop(cancel_tx);
        assert_eq!(result.state, RemoteCommandState::Failed);
        assert!(result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("dirty before execution")));
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
