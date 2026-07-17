//! Daemon-local hosted-control leases.
//!
//! Connect and fleet routing remain availability/metadata services. This
//! module owns the only authority-bearing state for the optional fleet-name
//! lane: signed doorbell requests, short-lived lease principals, compiled
//! presets, proof replay bounds, and one-use WebSocket tickets.

mod model;
mod policy;
mod runtime;

pub use model::*;
pub use policy::*;
pub use runtime::*;
