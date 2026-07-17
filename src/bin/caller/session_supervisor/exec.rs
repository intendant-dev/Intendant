//! Off-intake execution for the session supervisor: per-session ordered
//! job queues with a small global concurrency bound for heavy launch
//! bodies, plus the pending-route / pending-delegation reservations that
//! keep a launching session addressable — and its commands ordered —
//! while its slow body (create / resume / restart / fork / delegation
//! spawn) executes off the control-intake loop.
//!
//! The intake loop (`run_event_loop`) stays strictly sequential and
//! lossless; what changed is WHERE command bodies run. `dispatch_control_msg`
//! (dispatch.rs) reserves identity synchronously at intake and hands slow
//! bodies here, so one session's multi-second worktree checkout no longer
//! blocks another session's approvals / steers / interrupts. Ordering
//! contract: all jobs enqueued under one key run strictly in enqueue
//! order, one at a time; different keys run concurrently (heavy bodies
//! additionally bounded by [`MAX_CONCURRENT_LAUNCH_BODIES`]).

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// Global bound on concurrently executing heavy launch bodies (session
/// create / resume / restart / fork / delegation spawns). Deferred fast
/// commands queued behind them are not counted — only the bodies that do
/// real work (log open, project load, checkout, process spawn) occupy a
/// slot. Matches the git-ops bound's scale: enough parallelism that a
/// burst of dashboard launches overlaps, small enough that a stampede
/// cannot swamp the blocking pool and disk.
pub(crate) const MAX_CONCURRENT_LAUNCH_BODIES: usize = 4;

pub(crate) type IntakeJobFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// One unit of deferred intake work for a session key.
pub(crate) struct IntakeJob {
    fut: IntakeJobFuture,
    /// Heavy jobs (launch bodies) take a [`MAX_CONCURRENT_LAUNCH_BODIES`]
    /// permit before running; deferred fast commands do not.
    heavy: bool,
    /// Ids reserved as pending routes for this job (released when the job
    /// settles — completes OR panics). While held, `route_key` maps each
    /// id to this job's queue so later commands for the same session
    /// defer behind it instead of racing it.
    routes: Vec<String>,
    /// Pending peer-delegation reservation (released at settle). While
    /// held, a duplicate delivery of the same delegation id is routed
    /// onto this job's queue instead of starting a second task.
    delegation_id: Option<String>,
}

impl IntakeJob {
    /// A deferred fast command: no reservations, no launch permit.
    pub(crate) fn light(fut: IntakeJobFuture) -> Self {
        Self {
            fut,
            heavy: false,
            routes: Vec::new(),
            delegation_id: None,
        }
    }

    /// A slow launch body with its intake-reserved routes (and, for peer
    /// delegations, the delegation-id reservation).
    pub(crate) fn heavy(
        fut: IntakeJobFuture,
        routes: Vec<String>,
        delegation_id: Option<String>,
    ) -> Self {
        Self {
            fut,
            heavy: true,
            routes,
            delegation_id,
        }
    }
}

/// A reservation entry shared by overlapping jobs: refcounted so two
/// queued jobs reserving the same id (two resumes of one session) release
/// independently without dropping each other's route early.
struct Reservation {
    key: String,
    refs: usize,
}

#[derive(Default)]
struct QueueEntry {
    jobs: VecDeque<IntakeJob>,
    worker_running: bool,
}

#[derive(Default)]
struct ExecState {
    /// Session key → its ordered queue. An entry exists exactly while the
    /// key is busy (worker running or jobs pending); it is removed when
    /// the queue drains, which is what ends `queue_busy`.
    queues: HashMap<String, QueueEntry>,
    /// Pending routes: any id a not-yet-settled slow job answers for →
    /// that job's queue key.
    routes: HashMap<String, Reservation>,
    /// Pending peer delegations: delegation id → the queue key of the
    /// launch that will (or did) dispatch it.
    delegations: HashMap<String, Reservation>,
    /// Queue keys of not-yet-settled heavy jobs in enqueue order. An
    /// untargeted command (the legacy active-session fallback) chases the
    /// newest entry, approximating the serial intake where it would have
    /// processed after every earlier launch.
    pending_heavy_keys: Vec<String>,
}

pub(crate) struct IntakeExecutor {
    state: Mutex<ExecState>,
    launch_permits: Arc<tokio::sync::Semaphore>,
}

impl IntakeExecutor {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ExecState::default()),
            launch_permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_LAUNCH_BODIES)),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ExecState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True while `key` has a worker running or jobs pending. A command
    /// for a busy key must defer onto the queue to keep the session's
    /// command order.
    pub(crate) fn queue_busy(&self, key: &str) -> bool {
        self.lock().queues.contains_key(key)
    }

    /// The queue key a pending (not-yet-settled) slow job reserved for
    /// `id`, if any.
    pub(crate) fn route_key(&self, id: &str) -> Option<String> {
        self.lock().routes.get(id).map(|r| r.key.clone())
    }

    pub(crate) fn has_route(&self, id: &str) -> bool {
        self.lock().routes.contains_key(id)
    }

    /// The queue key of the newest not-yet-settled heavy job, if any.
    pub(crate) fn latest_pending_heavy_key(&self) -> Option<String> {
        self.lock().pending_heavy_keys.last().cloned()
    }

    /// The queue key of a pending (not-yet-settled) launch that reserved
    /// this peer-delegation id, if any.
    pub(crate) fn pending_delegation_key(&self, delegation_id: &str) -> Option<String> {
        self.lock().delegations.get(delegation_id).map(|r| r.key.clone())
    }

    #[cfg(test)]
    pub(crate) fn pending_route_count(&self) -> usize {
        self.lock().routes.len()
    }

    #[cfg(test)]
    pub(crate) fn pending_route_ids(&self) -> Vec<String> {
        self.lock().routes.keys().cloned().collect()
    }

    /// Enqueue a job under `key`, registering its reservations in the
    /// same lock so a later intake sees the routes exactly when the job
    /// becomes observable. Spawns the key's worker when none is running.
    pub(crate) fn enqueue(self: &Arc<Self>, key: &str, job: IntakeJob) {
        let spawn_worker = {
            let mut state = self.lock();
            for id in &job.routes {
                reserve(&mut state.routes, id, key);
            }
            if let Some(delegation_id) = &job.delegation_id {
                reserve(&mut state.delegations, delegation_id, key);
            }
            if job.heavy {
                state.pending_heavy_keys.push(key.to_string());
            }
            let entry = state.queues.entry(key.to_string()).or_default();
            entry.jobs.push_back(job);
            if entry.worker_running {
                false
            } else {
                entry.worker_running = true;
                true
            }
        };
        if spawn_worker {
            let executor = self.clone();
            let key = key.to_string();
            tokio::spawn(async move { executor.run_worker(key).await });
        }
    }

    /// Drain one key's queue in order. Each job runs in its own spawned
    /// task so a panic fails that session's command without wedging the
    /// queue (or unwinding the intake loop, which a panicking launch body
    /// did when it was awaited inline). Reservations release when the job
    /// settles, success or panic.
    async fn run_worker(self: Arc<Self>, key: String) {
        loop {
            let job = {
                let mut state = self.lock();
                let Some(entry) = state.queues.get_mut(&key) else {
                    return;
                };
                match entry.jobs.pop_front() {
                    Some(job) => job,
                    None => {
                        state.queues.remove(&key);
                        return;
                    }
                }
            };
            let IntakeJob {
                fut,
                heavy,
                routes,
                delegation_id,
            } = job;
            let permit = if heavy {
                // The semaphore is never closed; if that ever changes,
                // running unbounded beats wedging the session's queue.
                self.launch_permits.clone().acquire_owned().await.ok()
            } else {
                None
            };
            let settled = tokio::spawn(async move {
                let _permit = permit;
                fut.await;
            })
            .await;
            if settled.is_err() {
                eprintln!(
                    "[supervisor] queued command for session {key} panicked; \
                     continuing with the session's remaining commands"
                );
            }
            let mut state = self.lock();
            for id in &routes {
                release(&mut state.routes, id, &key);
            }
            if let Some(delegation_id) = &delegation_id {
                release(&mut state.delegations, delegation_id, &key);
            }
            if heavy {
                if let Some(pos) = state.pending_heavy_keys.iter().position(|k| k == &key) {
                    state.pending_heavy_keys.remove(pos);
                }
            }
        }
    }
}

fn reserve(map: &mut HashMap<String, Reservation>, id: &str, key: &str) {
    let id = id.trim();
    if id.is_empty() {
        return;
    }
    match map.get_mut(id) {
        Some(existing) => {
            // The dispatcher keys overlapping jobs onto the existing
            // route's queue, so a second reservation for an id always
            // names the same key; refcount it so releases pair up.
            debug_assert_eq!(existing.key, key, "route re-reserved under a different key");
            existing.refs += 1;
        }
        None => {
            map.insert(
                id.to_string(),
                Reservation {
                    key: key.to_string(),
                    refs: 1,
                },
            );
        }
    }
}

fn release(map: &mut HashMap<String, Reservation>, id: &str, key: &str) {
    let id = id.trim();
    if id.is_empty() {
        return;
    }
    if let Some(existing) = map.get_mut(id) {
        if existing.key != key {
            return;
        }
        existing.refs = existing.refs.saturating_sub(1);
        if existing.refs == 0 {
            map.remove(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn wait_until(deadline_ms: u64, mut check: impl FnMut() -> bool) -> bool {
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(deadline_ms);
        loop {
            if check() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// Jobs on one key run strictly in enqueue order; a held job on one
    /// key does not delay another key's job.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn per_key_order_with_cross_key_concurrency() {
        let executor = IntakeExecutor::new();
        let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);

        let log_a1 = log.clone();
        let mut gate = gate_rx.clone();
        executor.enqueue(
            "a",
            IntakeJob::heavy(
                Box::pin(async move {
                    let _ = gate.wait_for(|open| *open).await;
                    log_a1.lock().unwrap().push("a1");
                }),
                vec!["a".to_string()],
                None,
            ),
        );
        let log_a2 = log.clone();
        executor.enqueue(
            "a",
            IntakeJob::light(Box::pin(async move {
                log_a2.lock().unwrap().push("a2");
            })),
        );
        let log_b = log.clone();
        executor.enqueue(
            "b",
            IntakeJob::light(Box::pin(async move {
                log_b.lock().unwrap().push("b1");
            })),
        );

        // b's job completes while a's first job is still gated.
        assert!(
            wait_until(2000, || log.lock().unwrap().contains(&"b1")).await,
            "an independent key must not wait out a held job"
        );
        assert!(!log.lock().unwrap().contains(&"a1"));
        assert!(executor.queue_busy("a"));
        assert!(executor.has_route("a"), "route held while the job runs");

        gate_tx.send(true).unwrap();
        assert!(wait_until(2000, || !executor.queue_busy("a")).await);
        assert_eq!(*log.lock().unwrap(), vec!["b1", "a1", "a2"]);
        assert!(!executor.has_route("a"), "route released at settle");
        assert_eq!(executor.latest_pending_heavy_key(), None);
    }

    /// A panicking job settles like a completed one: reservations release
    /// and the key's remaining jobs still run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn panicked_job_releases_reservations_and_queue_survives() {
        let executor = IntakeExecutor::new();
        let ran_after = Arc::new(AtomicUsize::new(0));

        executor.enqueue(
            "a",
            IntakeJob::heavy(
                Box::pin(async move {
                    panic!("launch body exploded");
                }),
                vec!["a".to_string(), "alias-a".to_string()],
                Some("dg-1".to_string()),
            ),
        );
        let ran = ran_after.clone();
        executor.enqueue(
            "a",
            IntakeJob::light(Box::pin(async move {
                ran.fetch_add(1, Ordering::SeqCst);
            })),
        );

        assert!(
            wait_until(2000, || ran_after.load(Ordering::SeqCst) == 1).await,
            "the queue must survive a panicking job"
        );
        assert!(wait_until(2000, || !executor.queue_busy("a")).await);
        assert_eq!(executor.pending_route_count(), 0);
        assert_eq!(executor.pending_delegation_key("dg-1"), None);
        assert_eq!(executor.latest_pending_heavy_key(), None);
    }

    /// Overlapping reservations for the same id are refcounted: the first
    /// job's settle must not drop a route the second queued job still
    /// needs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn overlapping_route_reservations_are_refcounted() {
        let executor = IntakeExecutor::new();
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
        let first_done = Arc::new(AtomicUsize::new(0));

        let done = first_done.clone();
        executor.enqueue(
            "x",
            IntakeJob::heavy(
                Box::pin(async move {
                    done.fetch_add(1, Ordering::SeqCst);
                }),
                vec!["x".to_string()],
                None,
            ),
        );
        let mut gate = gate_rx.clone();
        executor.enqueue(
            "x",
            IntakeJob::heavy(
                Box::pin(async move {
                    let _ = gate.wait_for(|open| *open).await;
                }),
                vec!["x".to_string()],
                None,
            ),
        );

        assert!(wait_until(2000, || first_done.load(Ordering::SeqCst) == 1).await);
        assert!(
            executor.has_route("x"),
            "the second job's reservation must survive the first job's settle"
        );
        gate_tx.send(true).unwrap();
        assert!(wait_until(2000, || !executor.has_route("x")).await);
    }

    /// Heavy bodies respect the global launch bound; light jobs do not
    /// consume permits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn heavy_bodies_respect_global_launch_bound() {
        let executor = IntakeExecutor::new();
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);

        for i in 0..(MAX_CONCURRENT_LAUNCH_BODIES + 2) {
            let running = running.clone();
            let peak = peak.clone();
            let mut gate = gate_rx.clone();
            executor.enqueue(
                &format!("key-{i}"),
                IntakeJob::heavy(
                    Box::pin(async move {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        let _ = gate.wait_for(|open| *open).await;
                        running.fetch_sub(1, Ordering::SeqCst);
                    }),
                    vec![format!("key-{i}")],
                    None,
                ),
            );
        }

        assert!(
            wait_until(2000, || running.load(Ordering::SeqCst)
                == MAX_CONCURRENT_LAUNCH_BODIES)
            .await,
            "exactly the bound's worth of heavy bodies should start"
        );
        // Settle a moment: nothing beyond the bound starts while held.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(peak.load(Ordering::SeqCst), MAX_CONCURRENT_LAUNCH_BODIES);

        gate_tx.send(true).unwrap();
        assert!(wait_until(2000, || running.load(Ordering::SeqCst) == 0).await);
        assert!(peak.load(Ordering::SeqCst) <= MAX_CONCURRENT_LAUNCH_BODIES);
    }
}
