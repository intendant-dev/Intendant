//! Automatic Codex Cloud worker acquisition and honest idle retirement.
//!
//! The scheduler is provider-shaped only at this boundary. Callers ask for a
//! revision; we reuse a live matching attachment or coalesce one Cloud task
//! acquisition. Only workers created by this process are retired automatically.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{RemoteCommandAcquisitionStage, RemoteCommandAcquisitionView};

const DEFAULT_ACQUIRE_TIMEOUT_S: u64 = 3600;
const MAX_ACQUIRE_TIMEOUT_S: u64 = 7200;
const ENROLLMENT_TOKEN_GRACE_S: u64 = 300;
const PROVIDER_INITIAL_REFRESH_S: u64 = 15;
const PROVIDER_REFRESH_INTERVAL_S: u64 = 60;
const PROVIDER_REFRESH_TIMEOUT_S: u64 = 20;
const DEFAULT_IDLE_TIMEOUT_S: u64 = 600;
const REMOTE_WORKER_RETIRE_KIND: &str = "remote_worker_retire";

pub(super) type AcquisitionObserver =
    Arc<dyn Fn(RemoteCommandAcquisitionView) + Send + Sync + 'static>;

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
    progress: Mutex<Option<RemoteCommandAcquisitionView>>,
    observers: Mutex<Vec<FlightObserver>>,
}

struct FlightObserver {
    callback: AcquisitionObserver,
    coalesced: bool,
}

impl AcquireFlight {
    fn add_observer(&self, callback: AcquisitionObserver, coalesced: bool) {
        let current = {
            self.observers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(FlightObserver {
                    callback: Arc::clone(&callback),
                    coalesced,
                });
            self.progress
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        };
        if let Some(mut current) = current {
            current.coalesced = coalesced;
            callback(current);
        }
    }

    fn publish(&self, mut progress: RemoteCommandAcquisitionView) {
        progress.coalesced = false;
        *self
            .progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(progress.clone());
        let observers = self
            .observers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|observer| (Arc::clone(&observer.callback), observer.coalesced))
            .collect::<Vec<_>>();
        for (observer, coalesced) in observers {
            let mut update = progress.clone();
            update.coalesced = coalesced;
            observer(update);
        }
    }
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
    retire_retry_deadline: Instant,
}

static OWNED_WORKERS: OnceLock<Mutex<HashMap<String, OwnedWorker>>> = OnceLock::new();

fn owned_workers() -> &'static Mutex<HashMap<String, OwnedWorker>> {
    OWNED_WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn acquire_timeout_s() -> u64 {
    config_seconds(
        "INTENDANT_REMOTE_COMPUTE_ACQUIRE_TIMEOUT_S",
        DEFAULT_ACQUIRE_TIMEOUT_S,
        10,
        MAX_ACQUIRE_TIMEOUT_S,
    )
}

pub(super) fn select_branch(
    requested: Option<String>,
    derived: Option<String>,
) -> Result<Option<String>, String> {
    let selected = requested
        .or_else(|| optional_env("INTENDANT_REMOTE_COMPUTE_BRANCH"))
        .or(derived);
    selected
        .as_deref()
        .map(super::source::validate_provider_branch)
        .transpose()
}

pub(super) async fn acquire_worker(
    expected_revision: &str,
    branch: Option<String>,
    timeout_s: u64,
    observer: AcquisitionObserver,
) -> Result<AcquiredWorker, String> {
    let environment = required_env("INTENDANT_CODEX_CLOUD_ENVIRONMENT")?;
    let revision = expected_revision.trim().to_ascii_lowercase();
    let key = WorkerKey {
        environment,
        revision,
        branch,
    };
    let started_at_ms = crate::codex_cloud::now_unix_ms();
    let deadline_at_unix_ms = started_at_ms.saturating_add(timeout_s.saturating_mul(1000));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_s);
    observer(acquisition_progress(
        &key,
        RemoteCommandAcquisitionStage::CheckingForWorker,
        timeout_s,
        deadline_at_unix_ms,
        None,
        None,
        None,
    ));

    if let Some((worker, lease)) = matching_attached_worker(&key) {
        observer(acquisition_progress(
            &key,
            RemoteCommandAcquisitionStage::Attached,
            timeout_s,
            deadline_at_unix_ms,
            Some(&lease),
            Some(&lease.task_id),
            None,
        ));
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
                    progress: Mutex::new(None),
                    observers: Mutex::new(Vec::new()),
                });
                all.insert(key.clone(), Arc::clone(&flight));
                (flight, true)
            }
        }
    };
    flight.add_observer(observer, !leader);

    if leader {
        let result =
            acquire_new_worker(&key, timeout_s, deadline_at_unix_ms, deadline, &flight).await;
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

fn matching_attached_worker(
    key: &WorkerKey,
) -> Option<(AcquiredWorker, crate::codex_cloud::WorkerLease)> {
    crate::codex_cloud::cached_leases(&crate::codex_cloud::state_path())
        .ok()?
        .into_iter()
        .find(|lease| {
            let host = format!(
                "{}{}",
                crate::codex_cloud_attach::CLOUD_HOST_PREFIX,
                lease.task_id
            );
            crate::codex_cloud::lease_remote_compute_usable(lease)
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
                && !worker_is_retiring(&host)
        })
        .map(|lease| {
            let worker = AcquiredWorker {
                host: format!(
                    "{}{}",
                    crate::codex_cloud_attach::CLOUD_HOST_PREFIX,
                    lease.task_id
                ),
            };
            (worker, lease)
        })
}

async fn acquire_new_worker(
    key: &WorkerKey,
    timeout_s: u64,
    deadline_at_unix_ms: u64,
    deadline: tokio::time::Instant,
    flight: &AcquireFlight,
) -> Result<AcquiredWorker, String> {
    if let Some((worker, lease)) = matching_attached_worker(key) {
        flight.publish(acquisition_progress(
            key,
            RemoteCommandAcquisitionStage::Attached,
            timeout_s,
            deadline_at_unix_ms,
            Some(&lease),
            Some(&lease.task_id),
            None,
        ));
        return Ok(worker);
    }
    flight.publish(acquisition_progress(
        key,
        RemoteCommandAcquisitionStage::SubmittingTask,
        timeout_s,
        deadline_at_unix_ms,
        None,
        None,
        None,
    ));
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
        enrollment_token_ttl_s(timeout_s),
        crate::codex_cloud_attach::DEFAULT_IDENTITY_TTL_S,
        now_ms,
    )?;
    let prompt = crate::codex_cloud_attach::automatic_attach_prompt(
        &home_url,
        server_fingerprint.as_deref(),
        &token,
        tls_terminated_proxy,
    );
    let submitted = match tokio::time::timeout_at(
        deadline,
        crate::codex_cloud::submit_task(
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
        ),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            flight.publish(acquisition_progress(
                key,
                RemoteCommandAcquisitionStage::TimedOut,
                timeout_s,
                deadline_at_unix_ms,
                None,
                None,
                None,
            ));
            return Err(format!(
                "Codex Cloud did not finish submitting a worker before the {timeout_s}s acquisition deadline"
            ));
        }
    };
    let task_id = submitted.task_id.clone().ok_or_else(|| {
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
    register_owned(&host, timeout_s);
    let mut last_lease = cached_task_lease(&lease_store, &task_id).or(submitted.lease);
    let mut last_provider_error = None;
    flight.publish(acquisition_progress(
        key,
        RemoteCommandAcquisitionStage::WaitingForWorker,
        timeout_s,
        deadline_at_unix_ms,
        last_lease.as_ref(),
        Some(&task_id),
        None,
    ));
    let mut next_provider_refresh =
        tokio::time::Instant::now() + Duration::from_secs(PROVIDER_INITIAL_REFRESH_S);
    loop {
        if crate::codex_cloud_attach::attachment_channel(&task_id).is_some() {
            break;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            if let Some(lease) = last_lease.as_ref().filter(|lease| {
                lease.provider_state.is_terminal()
                    && crate::codex_cloud_attach::attachment_channel(&task_id).is_none()
            }) {
                flight.publish(acquisition_progress(
                    key,
                    RemoteCommandAcquisitionStage::ProviderEnded,
                    timeout_s,
                    deadline_at_unix_ms,
                    Some(lease),
                    Some(&task_id),
                    last_provider_error.as_deref(),
                ));
                let error = provider_ended_error(&task_id, lease);
                mark_idle(&host);
                return Err(error);
            }
            flight.publish(acquisition_progress(
                key,
                RemoteCommandAcquisitionStage::TimedOut,
                timeout_s,
                deadline_at_unix_ms,
                last_lease.as_ref(),
                Some(&task_id),
                last_provider_error.as_deref(),
            ));
            mark_idle(&host);
            return Err(acquisition_timeout_error(
                &task_id,
                timeout_s,
                last_lease.as_ref(),
                last_provider_error.as_deref(),
            ));
        }
        if now >= next_provider_refresh {
            match refresh_acquiring_task(&lease_store, key, &task_id).await {
                Ok(Some(lease)) => {
                    last_provider_error = None;
                    let terminal = lease.provider_state.is_terminal();
                    last_lease = Some(lease);
                    flight.publish(acquisition_progress(
                        key,
                        if terminal {
                            RemoteCommandAcquisitionStage::ProviderEnded
                        } else {
                            RemoteCommandAcquisitionStage::WaitingForWorker
                        },
                        timeout_s,
                        deadline_at_unix_ms,
                        last_lease.as_ref(),
                        Some(&task_id),
                        None,
                    ));
                    if terminal && crate::codex_cloud_attach::attachment_channel(&task_id).is_none()
                    {
                        mark_idle(&host);
                        return Err(provider_ended_error(
                            &task_id,
                            last_lease.as_ref().expect("lease was just stored"),
                        ));
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    last_provider_error = Some(concise_error(&error));
                    flight.publish(acquisition_progress(
                        key,
                        RemoteCommandAcquisitionStage::WaitingForWorker,
                        timeout_s,
                        deadline_at_unix_ms,
                        last_lease.as_ref(),
                        Some(&task_id),
                        last_provider_error.as_deref(),
                    ));
                }
            }
            next_provider_refresh =
                tokio::time::Instant::now() + Duration::from_secs(PROVIDER_REFRESH_INTERVAL_S);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    last_lease = cached_task_lease(&lease_store, &task_id).or(last_lease);
    flight.publish(acquisition_progress(
        key,
        RemoteCommandAcquisitionStage::Attached,
        timeout_s,
        deadline_at_unix_ms,
        last_lease.as_ref(),
        Some(&task_id),
        None,
    ));
    let actual_revision = last_lease
        .as_ref()
        .and_then(|lease| lease.worker.as_ref())
        .and_then(|worker| worker.git_rev.clone());
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

fn acquisition_progress(
    key: &WorkerKey,
    stage: RemoteCommandAcquisitionStage,
    timeout_s: u64,
    deadline_at_unix_ms: u64,
    lease: Option<&crate::codex_cloud::WorkerLease>,
    task_id: Option<&str>,
    last_provider_error: Option<&str>,
) -> RemoteCommandAcquisitionView {
    RemoteCommandAcquisitionView {
        stage,
        coalesced: false,
        branch: key.branch.clone(),
        task_id: task_id
            .map(str::to_string)
            .or_else(|| lease.map(|lease| lease.task_id.clone())),
        task_url: lease.and_then(|lease| lease.task_url.clone()),
        provider_status: lease.map(|lease| lease.provider_status.clone()),
        attachment_state: lease.map(|lease| lease.attachment_state.clone()),
        last_provider_error: last_provider_error.map(str::to_string),
        timeout_s,
        deadline_at_unix_ms,
    }
}

fn cached_task_lease(
    store_path: &std::path::Path,
    task_id: &str,
) -> Option<crate::codex_cloud::WorkerLease> {
    crate::codex_cloud::cached_leases(store_path)
        .ok()?
        .into_iter()
        .find(|lease| lease.task_id == task_id)
}

async fn refresh_acquiring_task(
    store_path: &std::path::Path,
    key: &WorkerKey,
    task_id: &str,
) -> Result<Option<crate::codex_cloud::WorkerLease>, String> {
    let outcome = tokio::time::timeout(
        Duration::from_secs(PROVIDER_REFRESH_TIMEOUT_S),
        crate::codex_cloud::refresh_leases(store_path, Some(&key.environment), 20, None),
    )
    .await
    .map_err(|_| format!("provider status refresh exceeded {PROVIDER_REFRESH_TIMEOUT_S}s"))??;
    crate::codex_cloud::announce_transitions(&outcome.transitions).await;
    Ok(outcome
        .workers
        .into_iter()
        .chain(outcome.tracked_active)
        .find(|lease| lease.task_id == task_id)
        .or_else(|| cached_task_lease(store_path, task_id)))
}

fn provider_ended_error(task_id: &str, lease: &crate::codex_cloud::WorkerLease) -> String {
    let url = lease
        .task_url
        .as_deref()
        .map(|url| format!(" Inspect {url}."))
        .unwrap_or_default();
    format!(
        "Codex Cloud task {task_id} reached provider status `{}` before its Intendant worker attached. The provider task has ended, so this is no longer a slow cold start.{url} Check the setup/maintenance output and the final attachment error.",
        lease.provider_status
    )
}

fn acquisition_timeout_error(
    task_id: &str,
    timeout_s: u64,
    lease: Option<&crate::codex_cloud::WorkerLease>,
    provider_error: Option<&str>,
) -> String {
    let status = lease
        .map(|lease| format!("; last provider status was `{}`", lease.provider_status))
        .unwrap_or_default();
    let url = lease
        .and_then(|lease| lease.task_url.as_deref())
        .map(|url| format!(" Inspect {url}."))
        .unwrap_or_default();
    let refresh = provider_error
        .map(|error| format!(" The last provider refresh failed: {error}."))
        .unwrap_or_default();
    format!(
        "Codex Cloud worker {task_id} did not attach before the {timeout_s}s acquisition deadline{status}. Intendant did not cancel the provider task; it may still be finishing cold setup.{url}{refresh} If this environment intentionally needs longer, raise INTENDANT_REMOTE_COMPUTE_ACQUIRE_TIMEOUT_S (maximum {MAX_ACQUIRE_TIMEOUT_S}s)."
    )
}

fn enrollment_token_ttl_s(timeout_s: u64) -> u64 {
    crate::codex_cloud_attach::DEFAULT_TOKEN_TTL_S
        .max(timeout_s.saturating_add(ENROLLMENT_TOKEN_GRACE_S))
}

fn concise_error(error: &str) -> String {
    const LIMIT: usize = 512;
    let mut concise = error.trim().chars().take(LIMIT).collect::<String>();
    if error.trim().chars().count() > LIMIT {
        concise.push('…');
    }
    concise
}

fn register_owned(host: &str, acquire_timeout_s: u64) {
    let mut workers = owned_workers()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // An unbound enrollment may redeem at the very end of a cold acquisition.
    // Keep retrying retirement until that latest possible identity has expired,
    // rather than measuring only from provider task creation.
    let retry_window_s = retire_retry_window_s(acquire_timeout_s);
    workers.insert(
        host.to_string(),
        OwnedWorker {
            // Acquisition itself is a use. This prevents a short configured
            // idle timeout from retiring a provider task before it attaches.
            active_jobs: 1,
            generation: 1,
            retiring: false,
            retire_retry_deadline: Instant::now() + Duration::from_secs(retry_window_s),
        },
    );
}

fn retire_retry_window_s(acquire_timeout_s: u64) -> u64 {
    enrollment_token_ttl_s(acquire_timeout_s)
        .saturating_add(crate::codex_cloud_attach::DEFAULT_IDENTITY_TTL_S)
        .saturating_add(60)
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
                        if Instant::now() >= worker.retire_retry_deadline {
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

    #[test]
    fn explicit_branch_outranks_the_derived_hint() {
        assert_eq!(
            select_branch(Some("feature/owner-call".into()), Some("main".into())).unwrap(),
            Some("feature/owner-call".into())
        );
        assert!(select_branch(Some("not..a-ref".into()), Some("main".into())).is_err());
    }

    #[test]
    fn enrollment_token_outlives_the_cold_acquisition_window() {
        assert_eq!(
            enrollment_token_ttl_s(100),
            crate::codex_cloud_attach::DEFAULT_TOKEN_TTL_S
        );
        assert_eq!(
            enrollment_token_ttl_s(DEFAULT_ACQUIRE_TIMEOUT_S),
            DEFAULT_ACQUIRE_TIMEOUT_S + ENROLLMENT_TOKEN_GRACE_S
        );
        assert_eq!(
            retire_retry_window_s(DEFAULT_ACQUIRE_TIMEOUT_S),
            DEFAULT_ACQUIRE_TIMEOUT_S
                + ENROLLMENT_TOKEN_GRACE_S
                + crate::codex_cloud_attach::DEFAULT_IDENTITY_TTL_S
                + 60
        );
    }

    #[test]
    fn coalesced_callers_receive_the_leaders_acquisition_progress() {
        let flight = AcquireFlight {
            result: Mutex::new(None),
            notify: tokio::sync::Notify::new(),
            progress: Mutex::new(None),
            observers: Mutex::new(Vec::new()),
        };
        let leader_seen = Arc::new(Mutex::new(Vec::new()));
        let follower_seen = Arc::new(Mutex::new(Vec::new()));
        for (seen, coalesced) in [
            (Arc::clone(&leader_seen), false),
            (Arc::clone(&follower_seen), true),
        ] {
            flight.add_observer(
                Arc::new(move |progress| {
                    seen.lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(progress);
                }),
                coalesced,
            );
        }
        let key = WorkerKey {
            environment: "env-a".into(),
            revision: "abc".into(),
            branch: Some("feature/worker".into()),
        };
        flight.publish(acquisition_progress(
            &key,
            RemoteCommandAcquisitionStage::WaitingForWorker,
            3_600,
            99_000,
            None,
            Some("task_e_shared"),
            None,
        ));

        let leader = leader_seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let follower = follower_seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(leader.len(), 1);
        assert_eq!(follower.len(), 1);
        assert!(!leader[0].coalesced);
        assert!(follower[0].coalesced);
        assert_eq!(follower[0].task_id.as_deref(), Some("task_e_shared"));
        assert_eq!(follower[0].deadline_at_unix_ms, 99_000);
    }
}
