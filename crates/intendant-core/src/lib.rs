//! Shared vocabulary for the intendant workspace: the caller error enum
//! (`error`) and the autonomy/approval model (`autonomy`). This crate is
//! a true leaf — it must not grow a dependency on any daemon subsystem;
//! things move *out* of the caller into here, never the other way.
//! (types.rs stays in the caller for now: its event payloads reference
//! peer/frontend/upload types that live above this crate.)

pub mod autonomy;
pub mod conversation;
pub mod error;
pub mod frames;
pub mod knowledge;
pub mod net;
pub mod skills;
pub mod state_paths;
pub mod usage;
pub mod vitals;
