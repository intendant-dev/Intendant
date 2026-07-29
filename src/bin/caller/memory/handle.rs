//! The daemon-side Memory authority. One [`MemoryHandle`] exists per
//! daemon process; every surface — HTTP route, dashboard tunnel twin,
//! MCP tool, ctl — funnels through it, which serializes writes under
//! one lock (the plane's single-writer contract). Storage is selected
//! at bootstrap: the primary OS normally uses the durable store while
//! other modes use an in-memory plane. Every view reports the
//! effective `durability`.
//!
//! Track HS4: durable-plane-open authority is LEASE-HOLDER-ONLY (the
//! design gate's Q6 amendment). The handle is a state machine
//! ([`PlaneSlot`]): a secondary never opens the durable store at all —
//! it serves a steady named refusal instead of the old silent lifetime
//! ephemeral fallback (whose durable-intent writes were lost on exit) —
//! and a draining daemon hands the plane over (drops the store, freeing
//! `plane.lock` for the successor's bounded-retry acquisition). Reads
//! AND writes refuse while the plane is not open here (Q5(b): serving
//! reads from a closed store is dishonest staleness).

use std::sync::Mutex;

use crate::event::{AppEvent, EventBus};

use super::service::MemoryService;

/// P1.8 storage-mode selector for [`MemoryHandle::bootstrap`].
pub(crate) enum MemoryStorage {
    Ephemeral,
    /// One-shot durable open. The daemon's durable lane goes through
    /// [`MemoryHandle::deferred`] + the role watch instead (HS4:
    /// lease-holder-only acquisition); this variant remains as the
    /// tests' lock-holder fixture and for direct tooling.
    #[allow(dead_code)]
    Durable(std::path::PathBuf),
}
use super::types::{ClaimView, JudgeArgs, MemoryError, ProposeArgs, SearchArgs};

/// The durable plane's home under a state root — one derivation for the
/// wiring edge and every test (the HS4 path fix: the old code resolved
/// `dirs::home_dir()/.intendant/memory-plane` directly, ignoring
/// `INTENDANT_HOME`, which made a dual-daemon rig unable to exercise
/// durable handover hermetically; the docs already framed that as an
/// inconsistency, not a policy).
pub(crate) fn durable_plane_dir(state_root: &std::path::Path) -> std::path::PathBuf {
    state_root.join("memory-plane")
}

/// What sits behind the handle right now (Track HS4).
enum PlaneSlot {
    /// A live plane (durable or ephemeral) — every op serves. Boxed:
    /// the service dwarfs the unit states.
    Ready(Box<MemoryService>),
    /// This daemon holds the lease and is acquiring the durable store
    /// (the bounded `LockDenied` retry runs in the role watch).
    Pending,
    /// This daemon is a co-homed secondary: the durable plane follows
    /// the lease holder (Q6 — never opened here).
    FollowsHolder,
    /// Drained: the store was dropped (releasing `plane.lock`) for the
    /// successor. One-way, like drain itself.
    HandedOver,
}

pub(crate) struct MemoryHandle {
    slot: Mutex<PlaneSlot>,
    /// Latched when a service installs; `"pending"` before that.
    plane_id_hex: Mutex<String>,
    bus: EventBus,
    /// Successor-pointer source for the named refusals (`None` outside
    /// the handover shapes and in plain bootstraps).
    handover: Option<std::sync::Arc<crate::handover::HandoverRuntime>>,
}

impl MemoryHandle {
    /// Bootstrap the plane (the full `c.genesis` ceremony, admitted by
    /// the stamped reducer) and wrap it single-writer. Admitted writes
    /// broadcast `memory_changed` on `bus` so every connected frontend
    /// updates live. `storage` picks the P1.8 mode: `Durable(dir)` on
    /// the proven-custody OS, `Ephemeral` elsewhere (and in tests).
    /// One-shot form (no lease coupling) — the daemon's durable lane
    /// uses [`MemoryHandle::deferred`] + [`super::spawn_plane_role_watch`]
    /// instead.
    pub(crate) fn bootstrap(
        bus: EventBus,
        storage: MemoryStorage,
    ) -> Result<MemoryHandle, MemoryError> {
        let service = match storage {
            MemoryStorage::Ephemeral => MemoryService::new()?,
            MemoryStorage::Durable(dir) => MemoryService::new_durable(&dir)
                .map_err(|e| MemoryError::InvalidArg(e.to_string()))?,
        };
        let plane_id_hex = service.plane_id_hex();
        Ok(MemoryHandle {
            slot: Mutex::new(PlaneSlot::Ready(Box::new(service))),
            plane_id_hex: Mutex::new(plane_id_hex),
            bus,
            handover: None,
        })
    }

    /// The lease-following form (Track HS4): starts without a plane —
    /// `Pending` when this boot already holds the lease, `FollowsHolder`
    /// otherwise — and the role watch installs/refuses from there.
    pub(crate) fn deferred(
        bus: EventBus,
        handover: std::sync::Arc<crate::handover::HandoverRuntime>,
    ) -> MemoryHandle {
        let initial = if handover.is_holder() {
            PlaneSlot::Pending
        } else {
            PlaneSlot::FollowsHolder
        };
        MemoryHandle {
            slot: Mutex::new(initial),
            plane_id_hex: Mutex::new("pending".to_string()),
            bus,
            handover: Some(handover),
        }
    }

    /// Install a live service (role watch: durable acquired, or the
    /// labeled ephemeral fallback for genuine store failure). No-op
    /// after a hand-over — drain is one-way.
    pub(crate) fn install_service(&self, service: MemoryService) {
        let mut slot = self.lock_slot();
        if matches!(*slot, PlaneSlot::HandedOver) {
            return;
        }
        *self.lock_plane_id() = service.plane_id_hex();
        *slot = PlaneSlot::Ready(Box::new(service));
    }

    /// Role watch: this daemon holds the lease and the durable open is
    /// in its retry window. No-op over `Ready` and after hand-over.
    pub(crate) fn set_pending(&self) {
        let mut slot = self.lock_slot();
        if matches!(*slot, PlaneSlot::FollowsHolder | PlaneSlot::Pending) {
            *slot = PlaneSlot::Pending;
        }
    }

    /// Role watch: this daemon is a secondary — the plane follows the
    /// holder (Q6: never opened here). No-op over `Ready` (a holder
    /// never demotes without draining) and after hand-over.
    pub(crate) fn set_follows_holder(&self) {
        let mut slot = self.lock_slot();
        if matches!(*slot, PlaneSlot::FollowsHolder | PlaneSlot::Pending) {
            *slot = PlaneSlot::FollowsHolder;
        }
    }

    /// Drain entry (§3.2 step 3, wired as a
    /// [`crate::handover::HandoverRuntime::on_drain_entry`] hook): drop
    /// the store — `plane.lock` frees HERE, before the scheduler lease's
    /// flock release — and refuse every subsequent op with the named
    /// `plane-handed-over`. One-way; idempotent.
    pub(crate) fn hand_over(&self) {
        let mut slot = self.lock_slot();
        if matches!(*slot, PlaneSlot::HandedOver) {
            return;
        }
        if matches!(*slot, PlaneSlot::Ready(_)) {
            eprintln!("[memory] durable plane released for the handover successor");
        }
        *slot = PlaneSlot::HandedOver;
    }

    /// The mode label views carry ("durable" / "ephemeral" / the HS4
    /// unavailability states).
    pub(crate) fn durability_label(&self) -> &'static str {
        match &*self.lock_slot() {
            PlaneSlot::Ready(service) => service.durability_label(),
            PlaneSlot::Pending => "pending",
            PlaneSlot::FollowsHolder => "follows-holder",
            PlaneSlot::HandedOver => "handed-over",
        }
    }

    pub(crate) fn plane_id_hex(&self) -> String {
        self.lock_plane_id().clone()
    }

    /// HS5 (the HS4 ruling's N3): a human-visible notice when the plane
    /// is not open on this daemon — "memory lives on :PORT" — for every
    /// surface that renders the durability label, not just the error
    /// lanes. `None` while serving.
    pub(crate) fn plane_notice(&self) -> Option<String> {
        let mut slot = self.lock_slot();
        match self.ready(&mut slot) {
            Ok(_) => None,
            Err(MemoryError::PlaneUnavailable { detail, .. }) => Some(detail),
            Err(_) => None,
        }
    }

    /// Author a claim. `actor` is the gate-resolved binding from the
    /// authenticated edge that dispatched this write (the seam
    /// contract in `access/actor.rs`) — the service maps it into the
    /// claim's own provenance fields and the op envelope's actor, and
    /// makes the ring authorization decision from it. Admission
    /// broadcasts the fresh view; rejections broadcast nothing.
    pub(crate) fn propose(
        &self,
        args: ProposeArgs,
        actor: &crate::access::actor::ActorBinding,
    ) -> Result<ClaimView, MemoryError> {
        let view = {
            let mut slot = self.lock_slot();
            self.ready(&mut slot)?.propose(args, actor)?
        };
        self.bus.send(AppEvent::MemoryChanged {
            claim: view.clone(),
        });
        Ok(view)
    }

    /// Judge a claim (owner curation — ruling R1: the service's
    /// judgment choke authorizes owner surfaces and denies ring-2
    /// with the named outcome). Admission broadcasts the target's
    /// refreshed view — derived status just moved (or a recorded
    /// non-counting judgment surfaced); rejections broadcast nothing.
    pub(crate) fn judge(
        &self,
        args: JudgeArgs,
        actor: &crate::access::actor::ActorBinding,
    ) -> Result<ClaimView, MemoryError> {
        let view = {
            let mut slot = self.lock_slot();
            self.ready(&mut slot)?.judge(args, actor)?
        };
        self.bus.send(AppEvent::MemoryChanged {
            claim: view.clone(),
        });
        Ok(view)
    }

    /// HS4: search refuses like every other op while the plane is not
    /// open here (Q5(b)); the infallible signature predates the state
    /// machine, so the refusal shape is an empty result — the label and
    /// the read/propose refusals carry the named state.
    pub(crate) fn search(&self, args: &SearchArgs) -> Vec<ClaimView> {
        match &mut *self.lock_slot() {
            PlaneSlot::Ready(service) => service.search(args),
            _ => Vec::new(),
        }
    }

    pub(crate) fn read(&self, id_prefix: &str) -> Result<ClaimView, MemoryError> {
        let mut slot = self.lock_slot();
        self.ready(&mut slot)?.read(id_prefix)
    }

    /// The Q6/Q5(b) gate every op passes: `Ready` serves; every other
    /// slot refuses with its named state and the successor pointer when
    /// the lease sidecar names one.
    fn ready<'a>(&self, slot: &'a mut PlaneSlot) -> Result<&'a mut MemoryService, MemoryError> {
        let outcome = match slot {
            PlaneSlot::Ready(service) => return Ok(service),
            PlaneSlot::Pending => "plane-pending",
            PlaneSlot::FollowsHolder => "plane-follows-holder",
            PlaneSlot::HandedOver => "plane-handed-over",
        };
        let successor = self
            .handover
            .as_ref()
            .and_then(|runtime| runtime.successor_port());
        let detail = match (outcome, successor) {
            ("plane-pending", _) => {
                "the durable plane is being acquired on this daemon — retry shortly".to_string()
            }
            (_, Some(port)) => {
                format!("the durable plane follows the active daemon — use :{port}")
            }
            (_, None) => "the durable plane follows the active daemon (not this one)".to_string(),
        };
        Err(MemoryError::PlaneUnavailable { outcome, detail })
    }

    fn lock_slot(&self) -> std::sync::MutexGuard<'_, PlaneSlot> {
        // Poison recovery is sound: the op log + fold cache are the
        // authority and every mutation re-derives the fold from the
        // full set, so a panicked writer cannot leave a half-applied
        // fold behind (worst case: an admitted claim missing from the
        // lexical registry, which the next propose does not disturb).
        match self.slot.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn lock_plane_id(&self) -> std::sync::MutexGuard<'_, String> {
        match self.plane_id_hex.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Fast-retry window for a `LockDenied` durable open (intake §3.4: the
/// predecessor's `plane.lock` frees at ITS drain entry — the successor's
/// bounded retry bridges the hop). Past the window the watch keeps
/// self-healing at the lease-poll cadence behind the steady named
/// `plane-pending` refusal — "another live daemon holds the plane" is
/// not "the store is corrupt", and only the latter deserves ephemeral.
const LOCK_DENIED_RETRY_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
const LOCK_DENIED_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_secs(2);

/// One decision of the lease-following plane watch — extracted from the
/// spawned loop so tests drive role transitions without sleeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaneRoleStep {
    /// Sleep the lease-poll cadence (secondary, or the slow self-heal
    /// lane past the fast window).
    Idle,
    /// Retry the durable open soon (inside the bounded window).
    RetrySoon,
    /// Steady state reached (service installed, or handed over) — the
    /// watch ends; drain hand-over is the drain hook's job from here.
    Settled,
}

/// The Q6 boot order, one decision at a time: acquire-lease-then-open-
/// plane. A secondary NEVER calls the durable open; a holder opens with
/// the bounded `LockDenied` retry; genuine store failure (corruption,
/// custody, IO) falls to the labeled ephemeral plane exactly as before.
pub(crate) fn plane_role_step(
    handle: &MemoryHandle,
    runtime: &crate::handover::HandoverRuntime,
    dir: &std::path::Path,
    lock_denied_since: &mut Option<std::time::Instant>,
) -> PlaneRoleStep {
    if runtime.is_draining() {
        handle.hand_over();
        return PlaneRoleStep::Settled;
    }
    if !runtime.is_holder() {
        handle.set_follows_holder();
        *lock_denied_since = None;
        return PlaneRoleStep::Idle;
    }
    match MemoryService::new_durable(dir) {
        Ok(service) => {
            println!(
                "[memory] durable plane {} at {}",
                &service.plane_id_hex()[..16],
                dir.display()
            );
            handle.install_service(service);
            PlaneRoleStep::Settled
        }
        Err(super::store::StoreError::LockDenied) => {
            handle.set_pending();
            let since = lock_denied_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() <= LOCK_DENIED_RETRY_WINDOW {
                PlaneRoleStep::RetrySoon
            } else {
                PlaneRoleStep::Idle
            }
        }
        Err(err) => {
            eprintln!("[memory] durable plane unavailable ({err}) — falling back to ephemeral");
            match MemoryService::new() {
                Ok(service) => {
                    println!(
                        "[memory] ephemeral plane {} (nothing persists across restarts)",
                        &service.plane_id_hex()[..16]
                    );
                    handle.install_service(service);
                }
                Err(err) => eprintln!("[memory] ephemeral plane bootstrap failed: {err}"),
            }
            PlaneRoleStep::Settled
        }
    }
}

/// The spawned watch: follows the lease until a steady state lands.
/// Detaches on drop like the wiring's sibling tasks.
pub(crate) fn spawn_plane_role_watch(
    handle: std::sync::Arc<MemoryHandle>,
    runtime: std::sync::Arc<crate::handover::HandoverRuntime>,
    dir: std::path::PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lock_denied_since = None;
        loop {
            match plane_role_step(&handle, &runtime, &dir, &mut lock_denied_since) {
                PlaneRoleStep::Settled => return,
                PlaneRoleStep::RetrySoon => tokio::time::sleep(LOCK_DENIED_RETRY_PAUSE).await,
                PlaneRoleStep::Idle => {
                    tokio::time::sleep(crate::handover::lease_poll_interval()).await
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::actor::ActorBinding;

    fn propose_args(statement: &str) -> ProposeArgs {
        ProposeArgs {
            kind: "observation".into(),
            statement: statement.into(),
            sensitivity: "private".into(),
            session: None,
            project: None,
            model: None,
            labels: vec![],
        }
    }

    /// The HS4 path fix: one derivation, state-root-relative — the
    /// wiring edge passes `intendant_home()`, so `INTENDANT_HOME` scopes
    /// the durable plane exactly like every other store (the old code
    /// resolved `dirs::home_dir()` directly and a dual-daemon rig could
    /// not exercise durable handover hermetically).
    #[test]
    fn durable_plane_honors_intendant_home() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            durable_plane_dir(root.path()),
            root.path().join("memory-plane")
        );
    }

    /// The Q6 amendment pin: a co-homed SECONDARY never opens the
    /// durable plane — the role step refuses without ever touching the
    /// store (the plane dir is not even created), and every op serves
    /// the named follows-holder refusal with the holder's port.
    #[tokio::test]
    async fn secondary_never_opens_durable_plane() {
        let home = tempfile::tempdir().unwrap();
        let plane = tempfile::tempdir().unwrap();
        let plane_dir = plane.path().join("memory-plane");
        let _holder = std::sync::Arc::new(crate::handover::HandoverRuntime::initialize(
            home.path(),
            7001,
            0,
        ));
        let secondary = std::sync::Arc::new(crate::handover::HandoverRuntime::initialize(
            home.path(),
            7002,
            0,
        ));
        let handle = MemoryHandle::deferred(EventBus::new(), secondary.clone());
        assert_eq!(handle.durability_label(), "follows-holder");

        let mut since = None;
        let step = plane_role_step(&handle, &secondary, &plane_dir, &mut since);
        assert_eq!(step, PlaneRoleStep::Idle);
        assert!(
            !plane_dir.exists(),
            "a secondary must never even create the plane dir"
        );
        let err = handle.read("anything").unwrap_err();
        match err {
            MemoryError::PlaneUnavailable { outcome, detail } => {
                assert_eq!(outcome, "plane-follows-holder");
                assert!(
                    detail.contains(":7001"),
                    "the holder's port rides: {detail}"
                );
            }
            other => panic!("expected the named refusal, got {other:?}"),
        }
    }

    /// The intake's bootstrap amendment: `LockDenied` means "another
    /// live daemon holds the plane", NOT "the store is corrupt" — the
    /// holder retries fast inside the bounded window, then keeps a
    /// steady named `plane-pending` refusal (slow self-heal), and never
    /// falls to the old silent lifetime ephemeral.
    #[tokio::test]
    async fn lock_denied_bootstrap_bounded_retry_then_named_refusal() {
        let home = tempfile::tempdir().unwrap();
        let plane = tempfile::tempdir().unwrap();
        let plane_dir = plane.path().join("memory-plane");
        // Another live handle holds the plane (the co-homed predecessor).
        let occupant =
            MemoryHandle::bootstrap(EventBus::new(), MemoryStorage::Durable(plane_dir.clone()))
                .expect("first durable open");

        let holder = std::sync::Arc::new(crate::handover::HandoverRuntime::initialize(
            home.path(),
            7001,
            0,
        ));
        let handle = MemoryHandle::deferred(EventBus::new(), holder.clone());
        let mut since = None;
        assert_eq!(
            plane_role_step(&handle, &holder, &plane_dir, &mut since),
            PlaneRoleStep::RetrySoon,
            "inside the bounded window: fast retry"
        );
        assert_eq!(handle.durability_label(), "pending");
        match handle.read("anything").unwrap_err() {
            MemoryError::PlaneUnavailable { outcome, .. } => {
                assert_eq!(outcome, "plane-pending")
            }
            other => panic!("expected plane-pending, got {other:?}"),
        }
        // Past the window: the steady named refusal with slow self-heal
        // (never ephemeral). Instant cannot always rewind on a fresh VM;
        // skip the aging assert when it cannot.
        if let Some(old) = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(61))
        {
            since = Some(old);
            assert_eq!(
                plane_role_step(&handle, &holder, &plane_dir, &mut since),
                PlaneRoleStep::Idle,
                "past the window: slow lane, still pending — never ephemeral"
            );
            assert_eq!(handle.durability_label(), "pending");
        }

        // The predecessor releases (its drain entry / exit): the very
        // next step acquires and the handle serves durably.
        drop(occupant);
        assert_eq!(
            plane_role_step(&handle, &holder, &plane_dir, &mut since),
            PlaneRoleStep::Settled
        );
        assert_eq!(handle.durability_label(), "durable");
        handle
            .propose(
                propose_args("acquired after the lock freed"),
                &ActorBinding::dashboard(Some("principal:root-session:test".into())),
            )
            .expect("the acquired plane serves writes");
    }

    /// The transfer pin: a draining holder's drain-entry hook releases
    /// the plane (plane.lock frees BEFORE the scheduler flock), the
    /// successor's role step acquires it within the retry window, the
    /// SAME plane resumes (identity and claims survive the hop), and the
    /// drainer's memory surface refuses with the successor pointer.
    #[tokio::test]
    async fn plane_transfers_within_retry_window_on_drain() {
        let home = tempfile::tempdir().unwrap();
        let plane = tempfile::tempdir().unwrap();
        let plane_dir = plane.path().join("memory-plane");

        let runtime_a = std::sync::Arc::new(crate::handover::HandoverRuntime::initialize(
            home.path(),
            7001,
            0,
        ));
        let handle_a =
            std::sync::Arc::new(MemoryHandle::deferred(EventBus::new(), runtime_a.clone()));
        let mut since_a = None;
        assert_eq!(
            plane_role_step(&handle_a, &runtime_a, &plane_dir, &mut since_a),
            PlaneRoleStep::Settled
        );
        assert_eq!(handle_a.durability_label(), "durable");
        let plane_id_a = handle_a.plane_id_hex();
        let claim = handle_a
            .propose(
                propose_args("written before the handover"),
                &ActorBinding::dashboard(Some("principal:root-session:test".into())),
            )
            .expect("durable write on the predecessor");

        // Drain: the hook releases the plane between the sidecar flip
        // and the flock release (§3.2 step 3).
        let hook_handle = handle_a.clone();
        runtime_a.on_drain_entry(Box::new(move || hook_handle.hand_over()));
        assert_eq!(
            runtime_a.request_drain(None),
            crate::handover::DrainRequest::Entered
        );
        assert_eq!(handle_a.durability_label(), "handed-over");

        // The successor: acquires the lease, then the plane.
        let runtime_b = std::sync::Arc::new(crate::handover::HandoverRuntime::initialize(
            home.path(),
            7002,
            0,
        ));
        assert!(
            runtime_b.is_holder(),
            "the freed lease transfers at the successor's boot try-acquire"
        );
        let handle_b = MemoryHandle::deferred(EventBus::new(), runtime_b.clone());
        let mut since_b = None;
        assert_eq!(
            plane_role_step(&handle_b, &runtime_b, &plane_dir, &mut since_b),
            PlaneRoleStep::Settled,
            "the freed plane.lock acquires on the first try"
        );
        assert_eq!(handle_b.durability_label(), "durable");
        assert_eq!(
            handle_b.plane_id_hex(),
            plane_id_a,
            "the SAME plane resumed — identity survives the hop"
        );
        let read_back = handle_b
            .read(&claim.id)
            .expect("the predecessor's claim survives the transfer");
        assert_eq!(read_back.statement, claim.statement);

        // The drainer's refusal now names the successor.
        match handle_a.read("anything").unwrap_err() {
            MemoryError::PlaneUnavailable { outcome, detail } => {
                assert_eq!(outcome, "plane-handed-over");
                assert!(
                    detail.contains(":7002"),
                    "successor pointer rides: {detail}"
                );
            }
            other => panic!("expected plane-handed-over, got {other:?}"),
        }
    }

    /// Admitted proposals broadcast `memory_changed` with the same view
    /// the caller received (the live-update lane the Explorer rides);
    /// rejected writes broadcast nothing.
    #[test]
    fn propose_broadcasts_memory_changed() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let handle = MemoryHandle::bootstrap(bus, MemoryStorage::Ephemeral).unwrap();

        let view = handle
            .propose(
                ProposeArgs {
                    kind: "observation".into(),
                    statement: "the explorer updates live".into(),
                    sensitivity: "private".into(),
                    session: None,
                    project: None,
                    model: None,
                    labels: vec![],
                },
                &ActorBinding::dashboard(Some("principal:root-session:test".into())),
            )
            .unwrap();

        match rx.try_recv() {
            Ok(AppEvent::MemoryChanged { claim }) => {
                assert_eq!(claim.id, view.id);
                assert_eq!(claim.proposed_by, view.proposed_by);
                assert_eq!(claim.durability, "ephemeral");
            }
            other => panic!("expected MemoryChanged, got {other:?}"),
        }

        // Rejections broadcast nothing.
        let err = handle.propose(
            ProposeArgs {
                kind: "fact".into(),
                statement: "unknown kind".into(),
                sensitivity: "private".into(),
                session: None,
                project: None,
                model: None,
                labels: vec![],
            },
            &ActorBinding::unattributed(),
        );
        assert!(err.is_err());
        assert!(
            rx.try_recv().is_err(),
            "a rejected propose must not broadcast"
        );
    }
}
