//! Agenda: the daemon-resident ledger where agents and the owner park
//! intent — deferred items, tasks, notes, later questions, reminders, and
//! owner-approved scheduled work.
//!
//! ## Shape (v1, ratified)
//!
//! - **Home-scoped, single daemon.** One agenda per daemon under
//!   `<state root>/agenda/` ([`agenda_dir`]) — deliberately *not*
//!   project-rooted: the agenda is one ledger of what's parked across all
//!   projects on this daemon. No sync, no zones, no Connect involvement.
//! - **Append-only JSONL op log.** One [`types::AgendaOpRecord`] per line;
//!   derived state is a fold over the log ([`types::apply_op`]); ops are
//!   never destroyed or rewritten — history is the diary's raw material.
//! - **The control plane is the single writer.** Frontends (ctl, dashboard,
//!   tunnel) emit intents ([`types::AgendaCommand`]); only the daemon
//!   validates, mints ids, appends, and folds.
//! - **Item bodies are data, never instructions** to any agent or surface
//!   that reads them (ratified doctrine; see `types::AgendaItem::body`).
//! - **Delivery and execution remain distinct.** `due_ms` schedules a
//!   reminder under owner-controlled delivery policy. A scheduled session
//!   is a separate effect object referencing an item (umbrella RFC §7.3),
//!   and runs only after an owner surface approves the exact manifest
//!   digest. Agents and peers may propose manifests but cannot approve or
//!   revoke them.
//!
//! The op vocabulary (`add`, `patch`, `complete`, `reopen`, `retire`,
//! `answer`, effect proposal/approval/revocation, and daemon-authored
//! occurrence records) tracks §7.2 of the owner-plane umbrella RFC so a
//! later D0-Agenda-Data gate can migrate this local log into the owner
//! plane without a vocabulary break.

mod ask;
mod blobs;
mod handle;
mod mandate_templates;
mod reminders;
mod scheduler;
mod spawn_project;
mod store;
mod types;

pub(crate) use ask::{agenda_ask_pending, ask_outcome_delivery_text, spawn_ask_resolver};
pub(crate) use blobs::find_blob;
pub(crate) use handle::AgendaHandle;
pub(crate) use reminders::{
    AgendaOccurrencesPage, ReminderPolicyPatch, AGENDA_OCCURRENCES_DEFAULT_LIMIT,
};
pub(crate) use scheduler::spawn_reminder_scheduler;
pub(crate) use spawn_project::{recorded_session_project_root, SessionSpawnContext};
pub(crate) use store::{
    file_ref_drift, AgendaError, AgendaOpsPage, AgendaStore, AGENDA_OPS_DEFAULT_LIMIT,
};
pub(crate) use types::{
    AgendaActor, AgendaAnswer, AgendaCommand, AgendaCounts, AgendaItem, AgendaKind, AgendaStatus,
};
// Test-support seam: cross-module tests (supervisor delivery, blocking
// waiter) drive real handle-side resolutions with structured content.
#[cfg(test)]
pub(crate) use ask::resolution_from_wire;
#[cfg(test)]
pub(crate) use types::AgendaAskResolution;

use std::path::{Path, PathBuf};

/// The agenda's home under an explicit state root (the testable seam — unit
/// tests thread tempdirs here, per the state_paths convention).
pub(crate) fn agenda_dir_in(state_root: &Path) -> PathBuf {
    state_root.join("agenda")
}

/// The daemon's agenda home: `~/.intendant/agenda` unless `$INTENDANT_HOME`
/// overrides the state root. Only the daemon edge resolves this; everything
/// under [`AgendaStore`] takes explicit paths.
pub(crate) fn agenda_dir() -> PathBuf {
    agenda_dir_in(&intendant_core::state_paths::intendant_home())
}
