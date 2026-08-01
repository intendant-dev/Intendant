//! Automatic Codex Cloud worker acquisition and honest idle retirement.
//!
//! The scheduler is provider-shaped only at this boundary. Callers ask for a
//! revision; we reuse a live matching attachment or coalesce one Cloud task
//! acquisition. Only workers created by this process are retired automatically.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const DEFAULT_ACQUIRE_TIMEOUT_S: u64 = 180;
const DEFAULT_IDLE_TIMEOUT_S: u64 = 600;
const REMOTE_WORKER_RETIRE_KIND: &str = "remote_worker_retire";

#[derive(Debug, Clone)]
pub(super) struct AcquiredWorker {
    pub host: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct WorkerKey {
    environment: String,
    revision: String,
    branch: Option<String>,
}

struct AcquireFlight {
    result: Mutex<Option<Result<AcquiredWorker, String>>>,
    notify: tokio::sync::Notify,
}

static FLIGHTS: OnceLock<Mutex<HashMap<WorkerKey, Arc<AcquireFlight>>>> = OnceLock::new();

fn flights() -> &'static Mutex<HashMap<WorkerKey, Arc<AcquireFlight>>> {
    FLIGHTS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug)]
struct OwnedWorker {
    active_jobs: usize,
    generation: u64,
    retiring: bool,
    created_at: Instant,
}

static OWNED_WORKERS: OnceLock<Mutex<HashMap<String, OwnedWorker>>> = OnceLock::new();

fn owned_workers() -> &'static Mutex<HashMap<String, OwnedWorker>> {
    OWNED_WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) async fn acquire_worker(
    expected_revision: &str,
    branch_hint: Option<String>,
) -> Result<AcquiredWorker, String> {
    let environment = required_env("INTENDANT_CODEX_CLOUD_ENVIRONMENT")?;
    let revision = expected_revision.trim().to_ascii_lowercase();
    let branch = optional_env("INTENDANT_REMOTE_COMPUTE_BRANCH").or(branch_hint);
    let key = WorkerKey {
        environment,
        revision,
        branch,
    };

    if let Some(worker) = matching_attached_worker(&key) {
        return Ok(worker);
    }

    let (flight, leader) = {
        let mut all = flights()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match all.get(&key) {
            Some(flight) => (Arc::clone(flight), false),
            None => {
                let flight = Arc::new(AcquireFlight {
                    result: Mutex::new(None),
                    notify: tokio::sync::Notify::new(),
                });
                all.insert(key.clone(), Arc::clone(&flight));
                (flight, true)
            }
        }
    };

    if leader {
        let result = acquire_new_worker(&key).await;
        *flight
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result.clone());
        flight.notify.notify_waiters();
        flights()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&key);
        return result;
    }

    loop {
        let notified = flight.notify.notified();
        if let Some(result) = flight
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            return result;
        }
        notified.await;
    }
}

fn matching_attached_worker(key: &WorkerKey) -> Option<AcquiredWorker> {
    crate::codex_cloud::cached_leases(&crate::codex_cloud::state_path())
        .ok()?
        .into_iter()
        .find(|lease| {
            let host = format!(
                "{}{}",
                crate::codex_cloud_attach::CLOUD_HOST_PREFIX,
                lease.task_id
            );
            lease.attachment_state == crate::codex_cloud::AttachmentState::Connected
                && lease.environment_id.as_deref() == Some(key.environment.as_str())
                && lease
                    .worker
                    .as_ref()
                    .and_then(|worker| worker.git_rev.as_deref())
                    .is_some_and(|revision| {
                        revision
                            .to_ascii_lowercase()
                            .starts_with(key.revision.as_str())
                    })
                && crate::codex_cloud_attach::attachment_channel(&lease.task_id).is_some()
                && !worker_is_retiring(&host)
        })
        .map(|lease| AcquiredWorker {
            host: format!(
                "{}{}",
                crate::codex_cloud_attach::CLOUD_HOST_PREFIX,
                lease.task_id
            ),
        })
}

async fn acquire_new_worker(key: &WorkerKey) -> Result<AcquiredWorker, String> {
    if let Some(worker) = matching_attached_worker(key) {
        return Ok(worker);
    }
    let home_url = crate::codex_cloud_attach::home_url_from(None)?;
    let tls_terminated_proxy = crate::codex_cloud_attach::tls_terminated_proxy_from_env();
    let server_fingerprint = if tls_terminated_proxy {
        None
    } else {
        let cert_dir = crate::access::backend::select_backend().cert_dir();
        Some(
            crate::access::certs::read_server_cert_fingerprint(&cert_dir).ok_or_else(|| {
                "automatic remote compute needs the daemon gateway TLS identity; start the daemon once and retry"
                    .to_string()
            })?,
        )
    };
    let lease_store = crate::codex_cloud::state_path();
    let broker = crate::codex_cloud_attach::broker_path(&lease_store);
    let now_ms = crate::codex_cloud::now_unix_ms();
    let (token, _) = crate::codex_cloud_attach::mint_unbound_enrollment(
        &broker,
        crate::codex_cloud_attach::DEFAULT_TOKEN_TTL_S,
        crate::codex_cloud_attach::DEFAULT_IDENTITY_TTL_S,
        now_ms,
    )?;
    let prompt = crate::codex_cloud_attach::automatic_attach_prompt(
        &home_url,
        server_fingerprint.as_deref(),
        &token,
        tls_terminated_proxy,
    );
    let submitted = crate::codex_cloud::submit_task(
        &lease_store,
        crate::codex_cloud::SubmitTaskRequest {
            environment: key.environment.clone(),
            branch: key.branch.clone(),
            attempts: 1,
            title: Some(format!(
                "Intendant remote worker {}",
                &key.revision[..key.revision.len().min(12)]
            )),
            prompt,
            probe: false,
        },
    )
    .await?;
    let task_id = submitted.task_id.ok_or_else(|| {
        "Codex Cloud accepted the worker task but returned no task id; automatic attachment cannot bind it"
            .to_string()
    })?;
    crate::codex_cloud_attach::bind_enrollment(
        &broker,
        &token,
        &task_id,
        crate::codex_cloud::now_unix_ms(),
    )?;
    let _ = crate::codex_cloud::record_attachment_state(
        &lease_store,
        &task_id,
        crate::codex_cloud::AttachmentState::Awaiting,
    );
    let host = format!("{}{task_id}", crate::codex_cloud_attach::CLOUD_HOST_PREFIX);
    // Ownership starts when this process creates the provider task, not only
    // after a successful attachment. A late attachment after our wait timeout
    // must still retire instead of becoming an unowned leak.
    register_owned(&host);

    let timeout = Duration::from_secs(config_seconds(
        "INTENDANT_REMOTE_COMPUTE_ACQUIRE_TIMEOUT_S",
        DEFAULT_ACQUIRE_TIMEOUT_S,
        10,
        900,
    ));
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if crate::codex_cloud_attach::attachment_channel(&task_id).is_some() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            mark_idle(&host);
            return Err(format!(
                "Codex Cloud worker {task_id} did not attach within {}s; verify the environment bootstrap and network allowlist",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let actual_revision = crate::codex_cloud::cached_leases(&lease_store)
        .ok()
        .and_then(|leases| {
            leases
                .into_iter()
                .find(|lease| lease.task_id == task_id)
                .and_then(|lease| lease.worker)
                .and_then(|worker| worker.git_rev)
        });
    if actual_revision.as_deref().is_none_or(|actual| {
        !actual
            .to_ascii_lowercase()
            .starts_with(key.revision.as_str())
    }) {
        mark_idle(&host);
        return Err(format!(
            "new Codex Cloud worker revision mismatch: expected {}, worker reported {}; push/select the intended revision or set INTENDANT_REMOTE_COMPUTE_BRANCH",
            key.revision,
            actual_revision.as_deref().unwrap_or("unknown")
        ));
    }

    mark_idle(&host);
    Ok(AcquiredWorker { host })
}

fn register_owned(host: &str) {
    let mut workers = owned_workers()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    workers.insert(
        host.to_string(),
        OwnedWorker {
            // Acquisition itself is a use. This prevents a short configured
            // idle timeout from retiring a provider task before it attaches.
            active_jobs: 1,
            generation: 1,
            retiring: false,
            created_at: Instant::now(),
        },
    );
}

pub(super) struct WorkerUse {
    host: String,
    owned: bool,
}

impl WorkerUse {
    pub fn begin(host: &str) -> Result<Self, String> {
        let owned = mark_active(host)?;
        Ok(Self {
            host: host.to_string(),
            owned,
        })
    }
}

impl Drop for WorkerUse {
    fn drop(&mut self) {
        if self.owned {
            mark_idle(&self.host);
        }
    }
}

fn mark_active(host: &str) -> Result<bool, String> {
    if let Some(worker) = owned_workers()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_mut(host)
    {
        if worker.retiring {
            return Err(format!(
                "remote host {host} began idle retirement; retry to acquire a live worker"
            ));
        }
        worker.active_jobs = worker.active_jobs.saturating_add(1);
        worker.generation = worker.generation.wrapping_add(1);
        Ok(true)
    } else {
        Ok(false)
    }
}

fn mark_idle(host: &str) {
    let generation = {
        let mut workers = owned_workers()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(worker) = workers.get_mut(host) else {
            return;
        };
        worker.active_jobs = worker.active_jobs.saturating_sub(1);
        if worker.active_jobs != 0 || worker.retiring {
            return;
        }
        worker.generation = worker.generation.wrapping_add(1);
        worker.generation
    };
    schedule_retirement(host.to_string(), generation);
}

fn schedule_retirement(host: String, generation: u64) {
    let idle_s = config_seconds(
        "INTENDANT_REMOTE_COMPUTE_IDLE_TIMEOUT_S",
        DEFAULT_IDLE_TIMEOUT_S,
        30,
        3600,
    );
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(idle_s)).await;
        let retire = {
            let mut workers = owned_workers()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(worker) = workers.get_mut(&host) else {
                return;
            };
            let matches =
                worker.active_jobs == 0 && worker.generation == generation && !worker.retiring;
            if matches {
                worker.retiring = true;
            }
            matches
        };
        if retire {
            if let Some(task_id) = crate::codex_cloud_attach::cloud_host_task_id(&host) {
                if retire_task(task_id).await {
                    // Keep a short tombstone so attachment discovery cannot
                    // hand the exiting worker to a new command.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    let mut workers = owned_workers()
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if workers.get(&host).is_some_and(|worker| {
                        worker.retiring
                            && worker.active_jobs == 0
                            && worker.generation == generation
                    }) {
                        workers.remove(&host);
                    }
                } else {
                    // A task that attaches after acquisition timed out still
                    // belongs to this daemon. Retry until its minted identity
                    // can no longer reconnect, instead of leaking it.
                    let retry_generation = {
                        let mut workers = owned_workers()
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let Some(worker) = workers.get_mut(&host) else {
                            return;
                        };
                        if worker.created_at.elapsed()
                            >= Duration::from_secs(
                                crate::codex_cloud_attach::DEFAULT_IDENTITY_TTL_S + 60,
                            )
                        {
                            workers.remove(&host);
                            return;
                        }
                        worker.retiring = false;
                        worker.generation = worker.generation.wrapping_add(1);
                        worker.generation
                    };
                    schedule_retirement(host, retry_generation);
                }
            }
        }
    });
}

async fn retire_task(task_id: &str) -> bool {
    let Some((to_worker, _)) = crate::codex_cloud_attach::attachment_channel(task_id) else {
        return false;
    };
    matches!(
        tokio::time::timeout(
            Duration::from_secs(5),
            to_worker.send(
                serde_json::json!({
                    "t": REMOTE_WORKER_RETIRE_KIND,
                    "host_id": format!(
                        "{}{task_id}",
                        crate::codex_cloud_attach::CLOUD_HOST_PREFIX
                    ),
                })
                .to_string(),
            ),
        )
        .await,
        Ok(Ok(()))
    )
}

pub(super) fn is_retire_frame(frame: &serde_json::Value) -> bool {
    frame.get("t").and_then(serde_json::Value::as_str) == Some(REMOTE_WORKER_RETIRE_KIND)
}

fn worker_is_retiring(host: &str) -> bool {
    owned_workers()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(host)
        .is_some_and(|worker| worker.retiring)
}

fn required_env(name: &str) -> Result<String, String> {
    optional_env(name).ok_or_else(|| {
        format!(
            "automatic remote compute requires {name} (the Codex Cloud environment id); pass an explicit cloud:<task-id> host or configure it"
        )
    })
}

fn optional_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn config_seconds(name: &str, default: u64, min: u64, max: u64) -> u64 {
    optional_env(name)
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| (min..=max).contains(value))
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_key_keeps_revision_environment_and_branch_distinct() {
        let first = WorkerKey {
            environment: "env-a".into(),
            revision: "abc".into(),
            branch: Some("main".into()),
        };
        let mut other = first.clone();
        other.revision = "def".into();
        assert_ne!(first, other);
        other = first.clone();
        other.environment = "env-b".into();
        assert_ne!(first, other);
        other = first.clone();
        other.branch = None;
        assert_ne!(first, other);
    }
}
