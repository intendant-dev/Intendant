//! The capacity admission gate (the supervisor side of `crate::capacity`):
//! gating create intents at the intake funnel, the deferred-admission
//! queue that fires when headroom returns, the park census under the park
//! stage, and the 5s capacity monitor that drives all of it.
//!
//! Fail-open shape: everything here is a no-op when the daemon runs
//! without a capacity controller (`config.capacity: None` — hermetic
//! tests, `[capacity] enabled = false`), which is exactly pre-slice
//! behavior.

use super::*;
use crate::capacity::{self, AdmissionCheck, CapacityStage};

/// One deferred admission: a create intent held with its minted
/// reservation until headroom returns. In-memory, like the intake it came
/// from — the durable retry custody for scheduled fires stays upstream in
/// the agenda scheduler, which holds occurrences instead of queueing here.
pub(crate) struct QueuedAdmission {
    msg: event::ControlMsg,
    reserved: Option<ReservedSessionLaunch>,
    enqueued_ms: u64,
}

impl QueuedAdmission {
    /// Wire row for the honest queue listing on the capacity view.
    fn queue_row(&self, position: usize) -> capacity::CapacityQueueRow {
        let kind = match &self.msg {
            event::ControlMsg::CreateSession { .. } => "create_session",
            event::ControlMsg::StartTask { .. } => "start_task",
            _ => "create",
        };
        capacity::CapacityQueueRow {
            position,
            kind: kind.to_string(),
            enqueued_ms: self.enqueued_ms,
        }
    }
}

impl SessionSupervisor {
    /// The capacity admission gate, immediately downstream of the drain
    /// gate. Returns the message (and reservation) when admitted; `None`
    /// when it was consumed — queued with an honest position, or refused
    /// with an honest reason. Lanes with their own retry custody (the
    /// agenda scheduler's delegation-tagged fires, peer delegations) and
    /// forks refuse; undelegated creates queue FIFO.
    pub(crate) async fn capacity_gate(
        &self,
        msg: event::ControlMsg,
        reserved: Option<ReservedSessionLaunch>,
    ) -> Option<(event::ControlMsg, Option<ReservedSessionLaunch>)> {
        let Some(controller) = self.config.capacity.clone() else {
            return Some((msg, reserved));
        };
        if !msg.creates_additional_session() {
            return Some((msg, reserved));
        }
        // A slash-command-shaped create routes as a follow-up into the
        // active session — it mints nothing, so it is not gated.
        let slow_create = match &msg {
            event::ControlMsg::CreateSession { task, .. }
            | event::ControlMsg::StartTask {
                session_id: None,
                task,
                ..
            } => matches!(
                dispatch::classify_create_task(task),
                dispatch::CreateDisposition::SlowCreate
            ),
            _ => true,
        };
        if !slow_create {
            return Some((msg, reserved));
        }
        enum Decision {
            Pass(event::ControlMsg, Option<ReservedSessionLaunch>),
            Refuse(String),
            Queued(String),
        }
        let decision = {
            let mut state = self.state.lock().await;
            // A duplicate delivery of an already-dispatched delegation
            // must reach the arm's re-ack branch, never a refusal — the
            // original session exists and answers for it.
            if let event::ControlMsg::StartTask {
                delegation_id: Some(id),
                ..
            } = &msg
            {
                if state.recorded_delegation_session(id).is_some() {
                    return Some((msg, reserved));
                }
            }
            let resident = state.sessions.len();
            match controller.admission_check(resident) {
                AdmissionCheck::Admit => Decision::Pass(msg, reserved),
                AdmissionCheck::Gate {
                    stage,
                    bound,
                    resident,
                    ..
                } => {
                    let queued = state.capacity_queue.len();
                    let refusing_lane = match &msg {
                        event::ControlMsg::ForkSessionAtAnchor { .. } => Some("session fork"),
                        event::ControlMsg::StartTask {
                            delegation_id: Some(_),
                            ..
                        } => Some("delegated task start"),
                        _ => None,
                    };
                    match refusing_lane {
                        Some(what) => Decision::Refuse(capacity::refusal_text(
                            what, stage, resident, bound, queued,
                        )),
                        None if queued >= capacity::ADMISSION_QUEUE_CAP => {
                            Decision::Refuse(capacity::refusal_text(
                                "session create (admission queue full)",
                                stage,
                                resident,
                                bound,
                                queued,
                            ))
                        }
                        None => {
                            state.capacity_queue.push_back(QueuedAdmission {
                                msg,
                                reserved,
                                enqueued_ms: epoch_ms_now(),
                            });
                            let position = state.capacity_queue.len();
                            Decision::Queued(capacity::queued_text(
                                "session create",
                                position,
                                stage,
                                resident,
                                bound,
                            ))
                        }
                    }
                }
            }
        };
        match decision {
            Decision::Pass(msg, reserved) => Some((msg, reserved)),
            Decision::Refuse(refusal) => {
                self.loop_error(refusal);
                self.publish_capacity_census().await;
                None
            }
            Decision::Queued(queued_notice) => {
                self.info(&queued_notice);
                self.publish_capacity_census().await;
                None
            }
        }
    }

    /// Release the queue head back into the intake when headroom exists.
    /// One admission per trigger (a `SessionEnded`, a monitor tick): the
    /// released launch registers before the next trigger re-checks, so a
    /// burst of freed slots cannot overshoot the bound by more than the
    /// intake's own launch concurrency.
    pub(crate) async fn drain_capacity_queue(&self) {
        if self.config.capacity.is_none() {
            return;
        }
        let released = {
            let controller = self.config.capacity.as_ref().expect("checked above");
            let mut state = self.state.lock().await;
            if state.capacity_queue.is_empty() {
                None
            } else {
                let resident = state.sessions.len();
                match controller.admission_check(resident) {
                    AdmissionCheck::Admit => state.capacity_queue.pop_front(),
                    AdmissionCheck::Gate { .. } => None,
                }
            }
        };
        if let Some(entry) = released {
            let waited_s = epoch_ms_now().saturating_sub(entry.enqueued_ms) / 1000;
            self.info(&format!(
                "capacity: headroom returned — firing queued session admission \
                 (waited {waited_s}s)"
            ));
            self.enqueue_reserved_create(entry.msg, entry.reserved).await;
        }
        self.publish_capacity_census().await;
    }

    /// Re-enter a previously queued create through the intake executor,
    /// preserving the reservation minted at its original intake (the
    /// session id it was promised). The gate re-checks on entry; a lost
    /// race re-queues honestly. Returns a boxed future (not an `async
    /// fn`) to break the opaque-type cycle — this body re-enters
    /// `handle_control_msg_with_reservation`, which reaches back here
    /// through the launch bodies (the `dispatch_control_msg` precedent).
    fn enqueue_reserved_create<'a>(
        &'a self,
        msg: event::ControlMsg,
        reserved: Option<ReservedSessionLaunch>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let reserved = reserved.unwrap_or_else(|| self.reserve_session_launch());
            let key = reserved.session_id.clone();
            let routes = vec![reserved.session_id.clone()];
            let supervisor = self.clone();
            self.exec.enqueue(
                &key,
                exec::IntakeJob::heavy(
                    Box::pin(async move {
                        supervisor
                            .handle_control_msg_with_reservation(msg, Some(reserved))
                            .await;
                    }),
                    routes,
                    None,
                ),
            );
        })
    }

    /// The park census under the park stage: longest-idle root sessions
    /// get an honest visible park mark; a session that shows activity
    /// again (the owner spoke to it), leaves, or any stage below park
    /// releases its mark. Marks never touch the session or its processes
    /// — they are the census of what is holding memory, and the promise
    /// that nothing auto-wakes it while pressure holds.
    async fn capacity_park_sweep(&self) {
        let Some(controller) = self.config.capacity.as_ref() else {
            return;
        };
        let stage = controller.view().stage;
        let now_s = epoch_ms_now() / 1000;
        let mut parked_delta: Vec<(String, bool)> = Vec::new();
        {
            let mut state = self.state.lock().await;
            if stage < CapacityStage::Park {
                for id in state.capacity_parked.drain() {
                    parked_delta.push((id, false));
                }
            } else {
                let mut keep: std::collections::HashSet<String> = std::collections::HashSet::new();
                let mut candidates: Vec<(u64, String)> = Vec::new();
                for (id, session) in state.sessions.iter() {
                    if session.depth != 0 {
                        continue;
                    }
                    if normalize_supervisor_phase(&session.phase) != "idle" {
                        continue;
                    }
                    let last_activity = crate::boot_readopt::activity_mtime_secs(
                        &session.session_dir,
                    );
                    let idle_for = now_s.saturating_sub(last_activity);
                    if state.capacity_parked.contains(id) {
                        keep.insert(id.clone());
                    } else if idle_for >= capacity::PARK_IDLE_MIN.as_secs() {
                        candidates.push((last_activity, id.clone()));
                    }
                }
                for id in state.capacity_parked.clone() {
                    if !keep.contains(&id) {
                        state.capacity_parked.remove(&id);
                        parked_delta.push((id, false));
                    }
                }
                // Longest-idle first: the census names the coldest work.
                candidates.sort();
                for (_, id) in candidates {
                    state.capacity_parked.insert(id.clone());
                    parked_delta.push((id, true));
                }
            }
        }
        for (id, parked) in parked_delta {
            if parked {
                self.info(&format!(
                    "capacity: parked idle session {id} (memory pressure — \
                     park stage; unparks when pressure eases or on activity)"
                ));
            } else {
                self.info(&format!("capacity: unparked session {id}"));
            }
        }
    }

    /// Recompute the resident/queue/park census on the controller and
    /// broadcast the capacity view when it changed.
    pub(crate) async fn publish_capacity_census(&self) {
        let Some(controller) = self.config.capacity.as_ref() else {
            return;
        };
        let (resident, queue, parked) = {
            let state = self.state.lock().await;
            let parked: Vec<String> = state.capacity_parked.iter().cloned().collect();
            let queue: Vec<capacity::CapacityQueueRow> = state
                .capacity_queue
                .iter()
                .enumerate()
                .map(|(i, entry)| entry.queue_row(i + 1))
                .collect();
            (state.sessions.len(), queue, parked)
        };
        if let Some(view) = controller.update_census(resident, queue, &parked) {
            self.config.bus.send(AppEvent::CapacityState { view });
        }
    }

    /// One capacity monitor beat: probe → stage fold → broadcast on
    /// change → park sweep → queue drain (which republishes the census).
    pub(crate) async fn capacity_monitor_tick(&self) {
        let Some(controller) = self.config.capacity.as_ref() else {
            return;
        };
        let sample = intendant_platform::memory::sample_memory();
        if let Some(view) = controller.observe(sample, std::time::Instant::now()) {
            self.config.bus.send(AppEvent::CapacityState { view });
        }
        self.capacity_park_sweep().await;
        self.drain_capacity_queue().await;
    }

    /// Spawn the capacity monitor (no-op without a controller) and publish
    /// the controller for the daemon-wide read surfaces (the agenda
    /// scheduler's level check, the MCP honesty twin, `get_status`).
    pub(crate) fn spawn_capacity_monitor(&self) -> Option<tokio::task::JoinHandle<()>> {
        let controller = self.config.capacity.clone()?;
        capacity::publish_capacity_controller(controller);
        let supervisor = self.clone();
        Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(capacity::CAPACITY_POLL_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                supervisor.capacity_monitor_tick().await;
            }
        }))
    }
}

fn epoch_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
