//! Damage-aware capture support — D-1 of #82 tile-streaming.
//!
//! This module is **infrastructure only** in D-1: it provides the
//! `DamageBackend` trait, the `DamageCapability` enum, and the X11
//! `XDamage`-based implementation. No code path consumes its output yet
//! — the existing VP8-q capture in [`super::x11`] is unchanged.
//! D-3 wires the damage stream into the tile encoder + transport.
//!
//! See `docs/design-tile-streaming.md` for the full architecture and
//! the must-fix design constraints (chunking, backpressure, synthetic
//! dirty rects, bounded recovery).

pub mod damage;
pub mod frame_diff;

#[cfg(target_os = "linux")]
pub mod x11_damage;
