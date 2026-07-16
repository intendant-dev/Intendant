//! The daemon-side Memory authority. One [`MemoryHandle`] exists per
//! daemon process; every surface — HTTP route, dashboard tunnel twin,
//! MCP tool, ctl — funnels through it, which serializes writes under
//! one lock (the single-writer contract for the ephemeral plane).
//! Mirrors `agenda::AgendaHandle`, minus persistence: the plane is
//! EPHEMERAL by the ratified P1 write bar, so there is no store dir,
//! no refresh-from-disk, and nothing survives a restart — every view
//! says so (`durability: "ephemeral"`).

use std::sync::Mutex;

use super::service::MemoryService;
use super::types::{ClaimView, MemoryError, ProposeArgs, SearchArgs};

pub(crate) struct MemoryHandle {
    service: Mutex<MemoryService>,
    plane_id_hex: String,
}

impl MemoryHandle {
    /// Bootstrap the ephemeral plane (the full `c.genesis` ceremony,
    /// admitted by the stamped reducer) and wrap it single-writer.
    pub(crate) fn bootstrap() -> Result<MemoryHandle, MemoryError> {
        let service = MemoryService::new()?;
        let plane_id_hex = service.plane_id_hex();
        Ok(MemoryHandle {
            service: Mutex::new(service),
            plane_id_hex,
        })
    }

    pub(crate) fn plane_id_hex(&self) -> &str {
        &self.plane_id_hex
    }

    pub(crate) fn propose(&self, args: ProposeArgs) -> Result<ClaimView, MemoryError> {
        self.lock().propose(args)
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
