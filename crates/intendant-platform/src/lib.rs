//! OS integration for the intendant workspace: platform probes, process
//! liveness/cmdline queries, spawn helpers (`platform`), the computer-use
//! display-target vocabulary (`display_target`), and virtual-display
//! lifecycle management (`vision`). Like intendant-core, this crate is a
//! leaf: things move *out* of the caller into here, never the other way.

pub mod display_target;
pub mod platform;
pub mod vision;

pub use display_target::DisplayTarget;
