//! Graceful daemon handover (Track HS). Co-homed daemons are the normal
//! dev topology on this codebase (worktree agent instances boot
//! constantly against one `~/.intendant`), and binary updates want a
//! successor daemon to take over standing automations without
//! kill-and-relaunch. The machinery, per the sealed design intake
//! (`daemon-handover-intake.md`, ruled 2026-07-26):
//!
//! - [`lease`] — the **active-scheduler lease**: an advisory flock
//!   (`scheduler-lease/holder.lock`) whose holder is the one daemon that
//!   runs standing automations, plus an observability sidecar
//!   (`lease.json`). Crash release is free; authority is the flock,
//!   never the JSON.
//! - [`presence`] — **per-boot presence files** (`daemons/<boot_id>.*`):
//!   liveness of any boot is "can I take its lock?", the substrate for
//!   scoped boot recovery (HS2) and the handover UI (HS3/HS5).
//! - [`HandoverRuntime`] — the process-wide view: this boot's identity,
//!   whether it holds the lease, and the status JSON `ctl status` and
//!   the dashboard serve.
//!
//! Slice map: HS1 (this module + journal generation stamping; behavior-
//! neutral), HS2 (firing gated on the lease + scoped boot recovery +
//! secondary poll-acquire), HS3 (drain + takeover), HS4 (memory-plane
//! transfer), HS5 (discovery handoff), HS6 (update surface).

mod lease;
mod presence;
mod update_watch;

pub(crate) use lease::{read_lease_sidecar, LeaseAttempt, SchedulerLease};
pub(crate) use presence::{boot_id_is_live, read_presence_records, DaemonPresence};
pub(crate) use update_watch::spawn_update_watch;

use std::path::{Path, PathBuf};

/// The process-wide handover state: one per gateway-shaped boot, shared
/// (via `Arc`) by the scheduler, the MCP status surface, and — in later
/// slices — the drain/takeover lanes.
pub(crate) struct HandoverRuntime {
    state_root: PathBuf,
    boot_id: String,
    port: u16,
    /// The held lease. `None` = secondary (another daemon holds it) or
    /// lease infrastructure failure (`lease_error` says which). Std
    /// mutex — never held across an await; the poll-acquire lane and
    /// the drain release mutate the slot.
    lease: std::sync::Mutex<Option<SchedulerLease>>,
    /// The most recent real I/O or lock-infrastructure failure ("held
    /// elsewhere" is NOT an error) — from the boot attempt or a later
    /// poll. Status surfaces carry it so a lockless filesystem degrades
    /// loudly, not silently; poll failures log only on message change so
    /// a broken filesystem cannot spam one line per poll.
    lease_error: std::sync::Mutex<Option<String>>,
    /// This boot's presence registration. Mutated through the drain
    /// lifecycle (`draining`/`exited` states, session counts); `None`
    /// when registration failed — which also DECLINES the lease (see
    /// [`Self::initialize`]: recovery authority depends on peers being
    /// able to probe the holder's liveness).
    presence: std::sync::Mutex<Option<DaemonPresence>>,
    presence_error: Option<String>,
    /// Drain is a one-way street (Q4: no draining→active edge, ever).
    /// The atomic is the hot-path answer every intent gate reads; the
    /// [`DrainState`] mutex carries the bookkeeping.
    draining: std::sync::atomic::AtomicBool,
    drain: std::sync::Mutex<DrainState>,
    /// Wakes the scheduler the instant a drain is requested, so
    /// drain-entry (which the scheduler performs BETWEEN passes — the
    /// structural "stop firing before the flock frees") does not wait
    /// out a sleep. Also the takeover lane's fast-poll wake on the
    /// successor side.
    drain_notify: tokio::sync::Notify,
    /// True once a scheduler attached ([`Self::attach_scheduler`]): drain
    /// entry is then the scheduler's duty (performed between passes so a
    /// firing pass can never straddle the release). Without a scheduler
    /// (agenda store failed — nothing fires), `request_drain` performs
    /// the entry inline.
    scheduler_attached: std::sync::atomic::AtomicBool,
    /// Drain-entry hooks (§3.2 step 3), run BETWEEN the sidecar's
    /// `draining` flip and the flock release — HS4's memory-plane
    /// release installs here. Hooks must be quick, idempotent, and
    /// infallible (failures are their own to log).
    drain_hooks: std::sync::Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
    /// The daemon event bus, installed at wiring (`set_bus`) once it
    /// exists: drain-entry and update notifications ride it, on EVERY
    /// entry path (HS3-N4 — the storeless inline entry was
    /// eprintln-only). Unset in bare test constructions: emissions then
    /// degrade to the log line.
    bus: std::sync::OnceLock<crate::event::EventBus>,
    /// The rendered update-surface block (HS6), owned by the update
    /// watch task; `status_json` serves it. `None` = the on-disk image
    /// is the running one (no chip).
    update_status: std::sync::Mutex<Option<serde_json::Value>>,
}

#[derive(Default)]
struct DrainState {
    /// Set by [`HandoverRuntime::request_drain`]; consumed into `entered`
    /// by the entry steps.
    requested: bool,
    /// When the ordered entry (§3.2 steps: stop firing → sidecar
    /// `draining` → flock release) completed.
    entered_ms: Option<u64>,
    /// Who asked (display currency for status/log lines).
    requested_by: Option<String>,
    /// The Q4 successor-gone alarm fired for this drain (ONE loud
    /// notification per episode).
    successor_gone_alerted: bool,
}

/// Outcome of a takeover/drain request, for the requesting surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrainRequest {
    /// Drain begun (or the entry was already pending) on this call.
    Entered,
    /// Already draining before this call — idempotent success.
    AlreadyDraining,
    /// This daemon does not hold the lease; there is nothing to hand
    /// over. The sidecar (status surfaces) names the actual holder.
    NotHolder,
}

impl HandoverRuntime {
    /// Mint this boot's identity, register presence, and try the lease
    /// once. Infallible by design: every failure degrades to a named
    /// field on the status surface and a log line, never a refused boot
    /// (HS1 is behavior-neutral — the daemon must run exactly as before).
    pub(crate) fn initialize(state_root: &Path, port: u16, journal_generation_floor: u64) -> Self {
        let boot_id = ulid::Ulid::new().to_string().to_lowercase();
        let (presence, presence_error) = match DaemonPresence::register(state_root, &boot_id, port)
        {
            Ok(presence) => (Some(presence), None),
            Err(err) => {
                eprintln!("[handover] presence registration failed: {err}");
                (None, Some(err.to_string()))
            }
        };
        // Presence-less daemons DECLINE the lease (HS2 ruling notable N1):
        // recovery scoping and the drain protocol both probe the holder's
        // per-boot lock for liveness — a holder nobody can probe reads as
        // dead, and peers would fail-close its in-flight rows while it
        // lives. No presence, no standing-automation authority; the
        // status surface says exactly why.
        let (held, lease_error) = if presence.is_none() {
            (
                None,
                Some(
                    "declined: presence registration failed — peers could not \
                     probe holder liveness"
                        .to_string(),
                ),
            )
        } else {
            match SchedulerLease::try_acquire(state_root, &boot_id, port, journal_generation_floor)
            {
                Ok(LeaseAttempt::Held(lease)) => {
                    println!(
                        "[handover] holding the scheduler lease (generation {})",
                        lease.generation()
                    );
                    write_holder_descriptor(state_root, port);
                    (Some(lease), None)
                }
                Ok(LeaseAttempt::HeldElsewhere(sidecar)) => {
                    match &sidecar {
                        Some(holder) => eprintln!(
                            "[handover] scheduler lease held by boot {} (pid {}, :{}) — \
                             running as secondary",
                            holder.boot_id, holder.pid, holder.port
                        ),
                        None => eprintln!(
                            "[handover] scheduler lease held by another daemon — \
                             running as secondary"
                        ),
                    }
                    (None, None)
                }
                Err(err) => {
                    eprintln!(
                        "[handover] scheduler lease unavailable ({err}) — \
                         running without the lease"
                    );
                    (None, Some(err.to_string()))
                }
            }
        };
        HandoverRuntime {
            state_root: state_root.to_path_buf(),
            boot_id,
            port,
            lease: std::sync::Mutex::new(held),
            lease_error: std::sync::Mutex::new(lease_error),
            presence: std::sync::Mutex::new(presence),
            presence_error,
            draining: std::sync::atomic::AtomicBool::new(false),
            drain: std::sync::Mutex::new(DrainState::default()),
            drain_notify: tokio::sync::Notify::new(),
            scheduler_attached: std::sync::atomic::AtomicBool::new(false),
            drain_hooks: std::sync::Mutex::new(Vec::new()),
            bus: std::sync::OnceLock::new(),
            update_status: std::sync::Mutex::new(None),
        }
    }

    /// Install the daemon event bus (wiring, once it exists). A second
    /// call is a no-op — the first bus wins.
    pub(crate) fn set_bus(&self, bus: crate::event::EventBus) {
        let _ = self.bus.set(bus);
    }

    /// Owner-visible notification through the daemon bus, when one is
    /// installed (bare test constructions degrade to nothing — callers
    /// carry their own log lines).
    pub(crate) fn notify_user(
        &self,
        id: &str,
        title: Option<&str>,
        text: &str,
        urgency: crate::types::NotificationUrgency,
    ) {
        if let Some(bus) = self.bus.get() {
            bus.send(crate::event::AppEvent::UserNotification {
                session_id: None,
                id: id.to_string(),
                title: title.map(str::to_string),
                text: text.to_string(),
                urgency,
                ts: now_ms(),
            });
        }
    }

    /// The update watch's rendered block (HS6); `None` clears the chip.
    pub(crate) fn set_update_status(&self, block: Option<serde_json::Value>) {
        if let Ok(mut slot) = self.update_status.lock() {
            *slot = block;
        }
    }

    pub(crate) fn boot_id(&self) -> &str {
        &self.boot_id
    }

    pub(crate) fn state_root(&self) -> &Path {
        &self.state_root
    }

    /// Does this daemon hold the active-scheduler lease right now?
    /// Standing automations (the scheduler firing pass, reminder
    /// delivery, the PR scanner) run iff this is true.
    pub(crate) fn is_holder(&self) -> bool {
        self.held_generation().is_some()
    }

    /// The held lease's generation; `None` while running as secondary.
    pub(crate) fn held_generation(&self) -> Option<u64> {
        self.lease
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(SchedulerLease::generation))
    }

    /// Secondary lane: one non-blocking attempt on the (possibly freed)
    /// lease. `true` = acquired on THIS call — the caller refreshes its
    /// journal stamp and turns standing automations on. Already-holding
    /// and still-held-elsewhere both return `false` without noise; real
    /// lock-infrastructure errors record on the status surface and log
    /// once per distinct message. A DRAINING daemon never acquires (Q4:
    /// drain is one-way), and a presence-less daemon never acquires
    /// (N1: peers must be able to probe the holder's liveness).
    pub(crate) fn poll_acquire(&self, journal_generation_floor: u64) -> bool {
        if self.is_draining() {
            return false;
        }
        if self
            .presence
            .lock()
            .map(|slot| slot.is_none())
            .unwrap_or(true)
        {
            return false;
        }
        {
            let Ok(slot) = self.lease.lock() else {
                return false;
            };
            if slot.is_some() {
                return false;
            }
        }
        // The attempt runs without the slot lock held (file I/O); the
        // scheduler is the only poll caller, so the slot cannot race.
        match SchedulerLease::try_acquire(
            &self.state_root,
            &self.boot_id,
            self.port,
            journal_generation_floor,
        ) {
            Ok(LeaseAttempt::Held(lease)) => {
                println!(
                    "[handover] scheduler lease acquired (generation {}) — \
                     standing automations on",
                    lease.generation()
                );
                write_holder_descriptor(&self.state_root, self.port);
                if let Ok(mut slot) = self.lease.lock() {
                    *slot = Some(lease);
                }
                if let Ok(mut last) = self.lease_error.lock() {
                    *last = None;
                }
                true
            }
            Ok(LeaseAttempt::HeldElsewhere(_)) => false,
            Err(err) => {
                let message = err.to_string();
                if let Ok(mut last) = self.lease_error.lock() {
                    if last.as_deref() != Some(message.as_str()) {
                        eprintln!("[handover] scheduler lease poll failed: {message}");
                        *last = Some(message);
                    }
                }
                false
            }
        }
    }

    /// Register a drain-entry hook (HS4+: the memory-plane release).
    /// Runs between the sidecar's `draining` flip and the flock release,
    /// in registration order. Late registration during an active drain
    /// runs the hook immediately — the entry already happened.
    pub(crate) fn on_drain_entry(&self, hook: Box<dyn Fn() + Send + Sync>) {
        let already_entered = self
            .drain
            .lock()
            .map(|drain| drain.entered_ms.is_some())
            .unwrap_or(false);
        if already_entered {
            hook();
            return;
        }
        if let Ok(mut hooks) = self.drain_hooks.lock() {
            hooks.push(hook);
        }
    }

    /// A scheduler attached: drain entry becomes its between-passes duty
    /// (a firing pass can then never straddle the flock release).
    pub(crate) fn attach_scheduler(&self) {
        self.scheduler_attached
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn is_draining(&self) -> bool {
        self.draining.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Resolves when a drain (or takeover fast-poll) wake is requested —
    /// the scheduler selects on this beside its sleep so entry never
    /// waits out a tick.
    pub(crate) async fn drain_wake(&self) {
        self.drain_notify.notified().await;
    }

    /// Wake the scheduler's select immediately (drain entry, the
    /// takeover fast-poll). Multi-purpose — the woken pass re-derives
    /// what to do from state.
    pub(crate) fn notify_wake(&self) {
        self.drain_notify.notify_one();
    }

    /// Request drain (the takeover lane's server side, and `ctl daemon
    /// takeover`). Idempotent; drain is one-way. With a scheduler
    /// attached the ordered entry runs on its next wake (notified here —
    /// milliseconds); without one (agenda store failed: nothing fires)
    /// the entry runs inline.
    pub(crate) fn request_drain(&self, requested_by: Option<String>) -> DrainRequest {
        if self.is_draining() {
            return DrainRequest::AlreadyDraining;
        }
        if !self.is_holder() {
            return DrainRequest::NotHolder;
        }
        if let Ok(mut drain) = self.drain.lock() {
            drain.requested = true;
            drain.requested_by = requested_by;
        }
        self.draining
            .store(true, std::sync::atomic::Ordering::Release);
        if self
            .scheduler_attached
            .load(std::sync::atomic::Ordering::Acquire)
        {
            self.drain_notify.notify_one();
        } else {
            self.perform_drain_entry();
        }
        DrainRequest::Entered
    }

    /// Scheduler side: claim the pending entry duty exactly once.
    pub(crate) fn take_drain_entry_duty(&self) -> bool {
        let Ok(mut drain) = self.drain.lock() else {
            return false;
        };
        if drain.requested && drain.entered_ms.is_none() {
            // Claimed; `perform_drain_entry` records `entered_ms`.
            drain.requested = false;
            true
        } else {
            false
        }
    }

    /// The ordered drain entry (§3.2): the caller has already stopped
    /// firing (scheduler: between passes; no scheduler: nothing fires).
    /// Steps here: sidecar → `draining` (still holding the flock, which
    /// serializes the write) → [HS4 hook: durable memory-plane release
    /// lands between these two steps] → flock release (the successor's
    /// acquire succeeds from this instant) → presence → `draining`.
    pub(crate) fn perform_drain_entry(&self) {
        let requested_by = self
            .drain
            .lock()
            .ok()
            .and_then(|drain| drain.requested_by.clone());
        if let Ok(mut slot) = self.lease.lock() {
            if let Some(mut lease) = slot.take() {
                if let Err(err) = lease.mark_draining(&self.state_root) {
                    eprintln!("[handover] drain sidecar write failed ({err}) — releasing anyway");
                }
                // §3.2 step 3 (HS4): the drain hooks run while the flock
                // is still ours — the memory hook drops the durable store
                // (freeing plane.lock) BEFORE the successor can acquire
                // the lease, so its bounded plane retry always races an
                // already-freed lock.
                if let Ok(hooks) = self.drain_hooks.lock() {
                    for hook in hooks.iter() {
                        hook();
                    }
                }
                drop(lease); // the flock frees HERE
            }
        }
        if let Ok(mut presence) = self.presence.lock() {
            if let Some(presence) = presence.as_mut() {
                let _ = presence.update_state("draining");
            }
        }
        if let Ok(mut drain) = self.drain.lock() {
            drain.requested = false;
            drain.entered_ms = Some(now_ms());
        }
        eprintln!(
            "[handover] draining: standing automations stopped, lease released{} — \
             serving in-flight sessions until the last ends",
            requested_by
                .map(|who| format!(" (requested by {who})"))
                .unwrap_or_default()
        );
        // Owner-visible on EVERY entry path (HS3-N4: the storeless
        // inline entry was eprintln-only — notification-invisible, and
        // its zero-session exit waited on the next unrelated bus
        // event). The event also lands on the supervisor's observation
        // lane, which re-evaluates the drain exit condition — covering
        // zero-sessions-at-entry on both paths.
        self.notify_user(
            "handover-draining",
            Some("Daemon draining"),
            "standing automations handed off; in-flight sessions finish here, \
             then this daemon exits",
            crate::types::NotificationUrgency::Attention,
        );
    }

    /// The Q4 successor watch, polled from the drainer's scheduler lane:
    /// once draining, if the lease sidecar names a successor whose boot
    /// is provably dead — or still names US past a grace window (the
    /// requester never acquired) — surface ONE loud alert: standing
    /// automations are paused machine-wide until someone relaunches or
    /// takes over. Returns the alert message exactly once per drain.
    pub(crate) fn drain_watch(&self) -> Option<String> {
        const SUCCESSOR_WAIT_GRACE_MS: u64 = 30_000;
        let entered_ms = {
            let drain = self.drain.lock().ok()?;
            if drain.successor_gone_alerted {
                return None;
            }
            drain.entered_ms?
        };
        let successor_gone = match read_lease_sidecar(&self.state_root) {
            Some(sidecar) if sidecar.boot_id != self.boot_id => {
                !boot_id_is_live(&self.state_root, &sidecar.boot_id)
            }
            // Still our own sidecar (or none): nobody acquired. Grace
            // covers the normal request→acquire hop.
            _ => now_ms().saturating_sub(entered_ms) > SUCCESSOR_WAIT_GRACE_MS,
        };
        if !successor_gone {
            return None;
        }
        if let Ok(mut drain) = self.drain.lock() {
            drain.successor_gone_alerted = true;
        }
        Some(
            "handover successor is gone — standing automations (schedules, \
             reminders, scans) are paused machine-wide; relaunch a daemon or \
             run a takeover"
                .to_string(),
        )
    }

    /// The successor's port for `daemon_draining` refusal pointers, when
    /// the sidecar already names one (None while the handoff is still in
    /// flight — the refusal then says "successor pending").
    pub(crate) fn successor_port(&self) -> Option<u16> {
        let sidecar = read_lease_sidecar(&self.state_root)?;
        (sidecar.boot_id != self.boot_id).then_some(sidecar.port)
    }

    /// Live supervised-session count, mirrored onto the presence record
    /// (the "draining · N sessions" surface).
    pub(crate) fn set_session_count(&self, count: u64) {
        if let Ok(mut presence) = self.presence.lock() {
            if let Some(presence) = presence.as_mut() {
                let _ = presence.update_session_count(count);
            }
        }
    }

    /// §3.2 step 6: the drainer's last session ended — record the
    /// terminal presence state; the process exits right after.
    pub(crate) fn mark_exited(&self) {
        if let Ok(mut presence) = self.presence.lock() {
            if let Some(presence) = presence.as_mut() {
                let _ = presence.update_state("exited");
            }
        }
        eprintln!("[handover] drained: last supervised session ended — exiting");
    }

    /// The `scheduler_lease` status block (`ctl status` / dashboard):
    /// this daemon's own view, the sidecar as it sits on disk
    /// (observability of whoever holds, honest `null` when nobody does),
    /// and every registered co-homed boot with its probed liveness — the
    /// dual-run topology made visible.
    pub(crate) fn status_json(&self) -> serde_json::Value {
        let (held, generation, acquired_at_ms) = match self.lease.lock() {
            Ok(slot) => match slot.as_ref() {
                Some(lease) => (
                    true,
                    Some(lease.generation()),
                    Some(lease.sidecar().acquired_at_ms),
                ),
                None => (false, None, None),
            },
            Err(_) => (false, None, None),
        };
        let daemons: Vec<serde_json::Value> = read_presence_records(&self.state_root)
            .into_iter()
            .map(|record| {
                let live = boot_id_is_live(&self.state_root, &record.boot_id);
                let mut value =
                    serde_json::to_value(&record).unwrap_or_else(|_| serde_json::json!({}));
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("live".into(), live.into());
                }
                value
            })
            .collect();
        let mut block = serde_json::json!({
            "boot_id": self.boot_id,
            "held": held,
            "draining": self.is_draining(),
            "sidecar": read_lease_sidecar(&self.state_root),
            "daemons": daemons,
        });
        let obj = block.as_object_mut().expect("literal object");
        if let Ok(drain) = self.drain.lock() {
            if let Some(entered_ms) = drain.entered_ms {
                obj.insert("drain_entered_ms".into(), entered_ms.into());
            }
            if drain.successor_gone_alerted {
                obj.insert("successor_gone".into(), true.into());
            }
        }
        if let Some(generation) = generation {
            obj.insert("generation".into(), generation.into());
        }
        if let Some(acquired_at_ms) = acquired_at_ms {
            obj.insert("acquired_at_ms".into(), acquired_at_ms.into());
        }
        if let Ok(last) = self.lease_error.lock() {
            if let Some(err) = last.as_ref() {
                obj.insert("error".into(), err.clone().into());
            }
        }
        if let Some(err) = &self.presence_error {
            obj.insert("presence_error".into(), err.clone().into());
        }
        if let Ok(update) = self.update_status.lock() {
            if let Some(update) = update.as_ref() {
                obj.insert("update".into(), update.clone());
            }
        }
        block
    }
}

/// Secondary poll cadence for a freed lease (crash convergence: when the
/// holder dies, the longest-standing daemon population converges on a new
/// holder within one interval, no owner action). The scheduler bounds its
/// idle sleep with this. `INTENDANT_LEASE_POLL_MS` overrides for rigs —
/// a two-daemon e2e cannot wait a minute per takeover; read once at
/// first use (a process-edge config read, like the memory kill switch).
pub(crate) fn lease_poll_interval() -> std::time::Duration {
    static INTERVAL: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *INTERVAL.get_or_init(|| {
        std::env::var("INTENDANT_LEASE_POLL_MS")
            .ok()
            .and_then(|raw| raw.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(std::time::Duration::from_millis)
            .unwrap_or(std::time::Duration::from_secs(60))
    })
}

/// The successor side of `--takeover` (intake §3.1, acquisition rule 3):
/// POST the holder's takeover route — the holder's port from the lease
/// sidecar, its per-port admission token from the loopback files, the
/// same-user trust class ctl rides — then fast-wake our own scheduler so
/// poll-acquire converges in milliseconds instead of a poll interval.
/// Bounded and infallible: on any failure this daemon simply remains an
/// ordinary polling secondary, and says so.
pub(crate) async fn run_takeover_request(
    runtime: std::sync::Arc<HandoverRuntime>,
    state_root: PathBuf,
) {
    if runtime.is_holder() {
        return; // boot already acquired a free lease — nothing to take over
    }
    let Some(sidecar) = read_lease_sidecar(&state_root) else {
        eprintln!("[handover] --takeover: no lease sidecar on this home — nothing to take over");
        return;
    };
    if sidecar.boot_id == runtime.boot_id() {
        return;
    }
    let port = sidecar.port;
    let scheme = std::fs::read_to_string(crate::loopback_token::loopback_sidecar_path(
        &state_root,
        port,
    ))
    .ok()
    .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
    .and_then(|meta| meta.get("scheme")?.as_str().map(str::to_string))
    .unwrap_or_else(|| "http".to_string());
    let token = std::fs::read_to_string(crate::loopback_token::loopback_token_path(
        &state_root,
        port,
    ))
    .map(|raw| raw.trim().to_string())
    .unwrap_or_default();
    // Loopback may serve the daemon's self-signed TLS cert: the per-port
    // admission token is the authority on this same-user lane, not WebPKI.
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build();
    let posted = match client {
        Ok(client) => {
            client
                .post(format!("{scheme}://127.0.0.1:{port}/api/daemon/takeover"))
                .header(crate::loopback_token::LOOPBACK_TOKEN_HEADER, token)
                .json(&serde_json::json!({
                    "requested_by": format!("--takeover boot {}", runtime.boot_id()),
                }))
                .send()
                .await
        }
        Err(err) => {
            eprintln!("[handover] --takeover: http client unavailable: {err}");
            return;
        }
    };
    match posted {
        Ok(response) if response.status().is_success() => {
            eprintln!("[handover] --takeover: holder on :{port} is draining");
        }
        Ok(response) => {
            eprintln!(
                "[handover] --takeover: holder on :{port} refused (HTTP {}) — \
                 continuing as polling secondary",
                response.status()
            );
            return;
        }
        Err(err) => {
            eprintln!(
                "[handover] --takeover: request to :{port} failed ({err}) — \
                 continuing as polling secondary"
            );
            return;
        }
    }
    // Fast convergence: wake the scheduler until the freed flock lands
    // (bounded — past this the ordinary poll cadence carries it).
    for _ in 0..40 {
        if runtime.is_holder() {
            eprintln!("[handover] --takeover: scheduler lease acquired");
            return;
        }
        runtime.notify_wake();
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    eprintln!(
        "[handover] --takeover: lease not acquired within the fast window — \
         continuing as polling secondary"
    );
}

/// Track HS5 (Q7): the CLI discovery descriptor follows the LEASE, not
/// the boot — the holder (re)writes it at every acquisition (boot
/// try-acquire, poll-acquire, takeover), so `cli-path.meta.json`'s port
/// always names the daemon running standing automations and secondaries
/// stop clobbering it. Failure only degrades discovery.
fn write_holder_descriptor(state_root: &Path, port: u16) {
    if let Err(err) = crate::cli_descriptor::write_boot_descriptor(state_root, port) {
        eprintln!("[cli-descriptor] not written at lease acquisition: {err}");
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Atomic replace, the `cli_descriptor` shape: readers never see a
/// partial file. Same-directory rename; the temp name is deterministic
/// per target, which is safe everywhere it is used (lease.json writes
/// are serialized by the flock; presence targets are per-boot).
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_registers_presence_and_takes_free_lease() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = HandoverRuntime::initialize(dir.path(), 8765, 0);
        assert_eq!(runtime.held_generation(), Some(1));
        assert!(boot_id_is_live(dir.path(), runtime.boot_id()));

        let status = runtime.status_json();
        assert_eq!(status["held"], true);
        assert_eq!(status["generation"], 1);
        assert_eq!(status["boot_id"], runtime.boot_id());
        assert_eq!(status["sidecar"]["boot_id"], runtime.boot_id());
        assert!(status.get("error").is_none());
        let daemons = status["daemons"].as_array().expect("daemons array");
        assert_eq!(daemons.len(), 1);
        assert_eq!(daemons[0]["boot_id"], runtime.boot_id());
        assert_eq!(daemons[0]["live"], true);
    }

    #[test]
    fn second_runtime_on_one_home_runs_as_secondary() {
        let dir = tempfile::tempdir().unwrap();
        let holder = HandoverRuntime::initialize(dir.path(), 8765, 0);
        let secondary = HandoverRuntime::initialize(dir.path(), 8766, 0);
        assert_eq!(secondary.held_generation(), None);

        let status = secondary.status_json();
        assert_eq!(status["held"], false);
        assert!(status.get("generation").is_none());
        // The sidecar still names the holder — that is the observability
        // contract a secondary's dashboard leans on.
        assert_eq!(status["sidecar"]["boot_id"], holder.boot_id());
        assert!(
            status.get("error").is_none(),
            "held-elsewhere is a role, not an error"
        );
        // Both boots are live presences (the dual-run topology, visible).
        assert!(boot_id_is_live(dir.path(), holder.boot_id()));
        assert!(boot_id_is_live(dir.path(), secondary.boot_id()));
    }

    /// Q7 (Track HS5): the CLI discovery descriptor follows the LEASE.
    /// A holder's acquisition writes it; a secondary's boot does NOT
    /// clobber it; a later acquisition (crash convergence or takeover)
    /// flips the meta port to the new holder.
    #[test]
    fn descriptor_rewrites_at_lease_acquisition_not_boot() {
        let dir = tempfile::tempdir().unwrap();
        let holder = HandoverRuntime::initialize(dir.path(), 7001, 0);
        assert_eq!(
            crate::cli_descriptor::meta_port(dir.path()),
            Some(7001),
            "the boot-acquiring holder writes the descriptor"
        );

        let secondary = HandoverRuntime::initialize(dir.path(), 7002, 0);
        assert_eq!(
            crate::cli_descriptor::meta_port(dir.path()),
            Some(7001),
            "a secondary boot never clobbers the holder's descriptor"
        );

        drop(holder);
        assert!(secondary.poll_acquire(0));
        assert_eq!(
            crate::cli_descriptor::meta_port(dir.path()),
            Some(7002),
            "acquisition flips the meta port to the new holder"
        );
    }

    #[test]
    fn poll_acquire_takes_a_freed_lease_and_noops_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let holder = HandoverRuntime::initialize(dir.path(), 8765, 0);
        let secondary = HandoverRuntime::initialize(dir.path(), 8766, 0);
        assert!(!secondary.poll_acquire(0), "held elsewhere: no acquisition");
        assert!(!holder.poll_acquire(0), "already holding: no-op");
        assert!(holder.is_holder());
        assert!(!secondary.is_holder());

        // Holder dies (drop = crash semantics): one poll converges.
        drop(holder);
        assert!(secondary.poll_acquire(5));
        assert!(secondary.is_holder());
        assert_eq!(
            secondary.held_generation(),
            Some(6),
            "the journal floor rule applies on the poll lane too"
        );
        assert!(!secondary.poll_acquire(0), "already holding after the poll");
    }

    /// §3.2 ordering, observable: request → sidecar flips to `draining`
    /// (still the drainer's) → flock frees → the successor's acquire
    /// succeeds with a bumped generation — and the drainer NEVER reclaims
    /// (Q4: drain is a one-way street).
    #[test]
    fn takeover_request_drains_then_transfers_flock() {
        let dir = tempfile::tempdir().unwrap();
        let holder = HandoverRuntime::initialize(dir.path(), 7001, 0);
        let successor = HandoverRuntime::initialize(dir.path(), 7002, 0);
        assert_eq!(
            successor.request_drain(None),
            DrainRequest::NotHolder,
            "only the holder has anything to hand over"
        );

        // No scheduler attached in this test: entry runs inline (the
        // attached path defers to the scheduler's next wake — pinned at
        // the scheduler level).
        assert_eq!(
            holder.request_drain(Some("test".into())),
            DrainRequest::Entered
        );
        assert!(holder.is_draining());
        assert!(!holder.is_holder(), "flock released at drain entry");
        let sidecar = read_lease_sidecar(dir.path()).expect("sidecar");
        assert_eq!(sidecar.state, "draining");
        assert_eq!(
            sidecar.boot_id,
            holder.boot_id(),
            "the draining sidecar still names the drainer until a successor acquires"
        );
        assert_eq!(
            holder.request_drain(None),
            DrainRequest::AlreadyDraining,
            "idempotent"
        );

        assert!(successor.poll_acquire(0), "freed flock transfers");
        assert_eq!(successor.held_generation(), Some(2));
        let sidecar = read_lease_sidecar(dir.path()).expect("sidecar");
        assert_eq!(sidecar.boot_id, successor.boot_id());
        assert_eq!(sidecar.state, "active");
        assert_eq!(
            holder.successor_port(),
            Some(7002),
            "refusal pointers carry the successor once the sidecar names it"
        );

        // The drainer never reclaims, even once the lease frees again.
        drop(successor);
        assert!(!holder.poll_acquire(0), "Q4: no draining→active edge");
        assert!(!holder.is_holder());
    }

    /// The Q4 pin: a drainer that observes its successor gone — crashed
    /// after acquiring — surfaces ONE loud alert (standing automations
    /// are paused machine-wide until someone acts).
    #[test]
    fn successor_crash_surfaces_paused_automations_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let drainer = HandoverRuntime::initialize(dir.path(), 7001, 0);
        let successor = HandoverRuntime::initialize(dir.path(), 7002, 0);
        assert_eq!(drainer.request_drain(None), DrainRequest::Entered);
        assert!(successor.poll_acquire(0));
        assert!(
            drainer.drain_watch().is_none(),
            "a live successor is a quiet drain"
        );

        drop(successor); // crash: presence lock frees, sidecar still names it
        let alert = drainer.drain_watch().expect("successor gone must alert");
        assert!(
            alert.contains("paused"),
            "the alert names the consequence: {alert}"
        );
        assert!(
            drainer.drain_watch().is_none(),
            "ONE loud notification per drain, never a spam loop"
        );
        let status = drainer.status_json();
        assert_eq!(status["draining"], true);
        assert_eq!(status["successor_gone"], true);
    }

    /// N1 (HS2 ruling): a daemon that cannot register presence never
    /// holds the lease — peers must be able to probe the holder's
    /// liveness or recovery would fail-close its in-flight rows.
    #[test]
    fn presence_less_daemon_declines_the_lease() {
        let dir = tempfile::tempdir().unwrap();
        // Occupy the daemons dir path with a FILE so registration fails
        // while the lease dir stays writable.
        std::fs::write(dir.path().join("daemons"), b"not a dir").unwrap();
        let runtime = HandoverRuntime::initialize(dir.path(), 7001, 0);
        assert!(!runtime.is_holder(), "declined, not acquired");
        assert!(!runtime.poll_acquire(0), "the poll lane declines too");
        let status = runtime.status_json();
        assert!(
            status["error"]
                .as_str()
                .is_some_and(|err| err.contains("declined")),
            "the status surface says why: {status}"
        );
    }

    #[test]
    fn dropped_runtime_releases_lease_and_liveness() {
        let dir = tempfile::tempdir().unwrap();
        let first = HandoverRuntime::initialize(dir.path(), 8765, 0);
        let first_boot = first.boot_id().to_string();
        drop(first);
        assert!(!boot_id_is_live(dir.path(), &first_boot));
        let second = HandoverRuntime::initialize(dir.path(), 8766, 0);
        assert_eq!(
            second.held_generation(),
            Some(2),
            "released lease reacquires with a bumped generation"
        );
    }

    /// HS3-N4 (folded into HS6): the INLINE drain entry — no scheduler
    /// attached, the storeless shape — emits the owner-visible
    /// "Daemon draining" notification through the bus, exactly like the
    /// scheduler-owned path (which now rides the same emission inside
    /// `perform_drain_entry`). The bus event is also what re-evaluates
    /// the supervisor's drain exit condition, so a zero-session
    /// storeless drain no longer waits on an unrelated event.
    #[test]
    fn inline_drain_entry_emits_the_owner_notification() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = HandoverRuntime::initialize(dir.path(), 7001, 0);
        assert!(runtime.is_holder());
        let bus = crate::event::EventBus::new();
        let mut events = bus.subscribe();
        runtime.set_bus(bus);
        // No `attach_scheduler` call: `request_drain` performs the
        // entry inline (the storeless shape).
        assert_eq!(runtime.request_drain(None), DrainRequest::Entered);
        let mut saw_draining_notification = false;
        while let Ok(event) = events.try_recv() {
            if let crate::event::AppEvent::UserNotification { id, urgency, .. } = event {
                if id == "handover-draining" {
                    assert_eq!(urgency, crate::types::NotificationUrgency::Attention);
                    saw_draining_notification = true;
                }
            }
        }
        assert!(
            saw_draining_notification,
            "the inline entry path must be notification-visible (HS3-N4)"
        );
    }

    /// HS6 conformance pin `spa_offers_reload_on_boot_id_change`: the
    /// config payload carries the daemon's boot id on every lane
    /// (serialized field + wiring assignment), and the shipped
    /// dashboard bundle wires it into the reload nudge — the config
    /// chokepoint feeds `maybeNudgeDaemonBoot`, whose banner offers the
    /// reload. The artifact scan pins the SPA half the same way the
    /// HTTP-map mirror test does.
    #[test]
    fn spa_offers_reload_on_boot_id_change() {
        let mut config = crate::web_gateway::WebGatewayConfig::default();
        config.boot_id = "boot-test".to_string();
        let serialized = serde_json::to_value(&config).expect("config serializes");
        assert_eq!(
            serialized["boot_id"], "boot-test",
            "the config payload must carry boot_id"
        );

        let app = include_str!("../../../../static/app.html");
        for needle in [
            // The nudge exists and the banner offers the reload…
            "function maybeNudgeDaemonBoot",
            "ui-daemon-boot-banner",
            // …and both boot_id lanes feed it: the config chokepoint
            // and the handover status poll.
            "maybeNudgeDaemonBoot(cfg.boot_id)",
            "maybeNudgeDaemonBoot(body.boot_id)",
        ] {
            assert!(
                app.contains(needle),
                "the dashboard bundle lost the boot-id reload wiring: {needle}"
            );
        }
    }
}
