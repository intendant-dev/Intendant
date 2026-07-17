//! The daemon-side Memory authority. One [`MemoryHandle`] exists per
//! daemon process; every surface — HTTP route, dashboard tunnel twin,
//! MCP tool, ctl — funnels through it, which serializes writes under
//! one lock (the single-writer contract for the ephemeral plane).
//! Mirrors `agenda::AgendaHandle`, minus persistence: the plane is
//! EPHEMERAL by the ratified P1 write bar, so there is no store dir,
//! no refresh-from-disk, and nothing survives a restart — every view
//! says so (`durability: "ephemeral"`).

use std::sync::Mutex;

use crate::event::{AppEvent, EventBus};

use super::service::MemoryService;

/// P1.8 storage-mode selector for [`MemoryHandle::bootstrap`].
pub(crate) enum MemoryStorage {
    Ephemeral,
    Durable(std::path::PathBuf),
}
use super::types::{ClaimView, MemoryError, ProposeArgs, SearchArgs};

pub(crate) struct MemoryHandle {
    service: Mutex<MemoryService>,
    plane_id_hex: String,
    bus: EventBus,
}

impl MemoryHandle {
    /// Bootstrap the plane (the full `c.genesis` ceremony, admitted by
    /// the stamped reducer) and wrap it single-writer. Admitted writes
    /// broadcast `memory_changed` on `bus` so every connected frontend
    /// updates live. `storage` picks the P1.8 mode: `Durable(dir)` on
    /// the proven-custody OS, `Ephemeral` elsewhere (and in tests).
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
            service: Mutex::new(service),
            plane_id_hex,
            bus,
        })
    }

    /// The mode label views carry ("durable" / "ephemeral").
    pub(crate) fn durability_label(&self) -> &'static str {
        self.lock().durability_label()
    }

    pub(crate) fn plane_id_hex(&self) -> &str {
        &self.plane_id_hex
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
        let view = self.lock().propose(args, actor)?;
        self.bus.send(AppEvent::MemoryChanged {
            claim: view.clone(),
        });
        Ok(view)
    }

    pub(crate) fn search(&self, args: &SearchArgs) -> Vec<ClaimView> {
        self.lock().search(args)
    }

    pub(crate) fn read(&self, id_prefix: &str) -> Result<ClaimView, MemoryError> {
        self.lock().read(id_prefix)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryService> {
        // Poison recovery is sound: the op log + fold cache are the
        // authority and every mutation re-derives the fold from the
        // full set, so a panicked writer cannot leave a half-applied
        // fold behind (worst case: an admitted claim missing from the
        // lexical registry, which the next propose does not disturb).
        match self.service.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::actor::ActorBinding;

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
