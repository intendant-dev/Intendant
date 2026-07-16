//! Agenda: the daemon-resident ledger where agents and the owner park
//! intent — deferred items, tasks, notes, later questions, reminders, and
//! (eventually) approved scheduled work.
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
//! - **Effects are a reserved seam.** Reminders/scheduled sessions arrive in
//!   later slices as *separate objects referencing items* (umbrella RFC §7.3),
//!   never as item fields. Nothing effectful exists in v1, and nothing here
//!   should grow effect fields on [`types::AgendaItem`].
//!
//! The op vocabulary (`add`, `patch`, `complete`, `reopen`, `retire`) tracks
//! §7.2 of the owner-plane umbrella RFC so a later D0-Agenda-Data gate can
//! migrate this local log into the owner plane without a vocabulary break.
//! The `question` kind and effect/occurrence operations are reserved there
//! and deliberately absent here.

mod handle;
mod reminders;
mod scheduler;
mod store;
mod types;

pub(crate) use handle::AgendaHandle;
pub(crate) use reminders::ReminderPolicyPatch;
pub(crate) use scheduler::spawn_reminder_scheduler;
pub(crate) use store::{AgendaError, AgendaStore};
pub(crate) use types::{AgendaActor, AgendaCommand, AgendaCounts, AgendaItem, AgendaStatus};

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
