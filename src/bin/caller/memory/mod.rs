//! The P1 Memory service — controller-owned and D0-A-format.
//!
//! Consumes the vendored owner-plane kernel: operations are minted
//! device-signed through `owner_plane_core` (the writer side) and
//! admitted through `owner_plane_reducer`'s fold (the reader side) —
//! write and admission stay independently implemented (the stamped
//! differential property), and claim status is derived by the
//! reducer's §11.2 status fold, never re-implemented here. Every
//! rejection surfaces the reducer's named outcome/disposition pair
//! verbatim (the D-203 §C.2 fail-closed contract: never a silent
//! proceed, never a downgrade).
//!
//! Storage is selected at daemon bootstrap. macOS uses the durable
//! Gate-B-lite store by default; other platforms, the
//! `INTENDANT_MEMORY_EPHEMERAL=1` kill switch, and a failed durable
//! bootstrap use an in-memory plane. Every view carries the effective
//! `durability` label, so a fallback is visible rather than silently
//! weakening the contract.
//!
//! This module is deliberately unrelated to the legacy
//! per-project `.intendant/memory.json` key-value system (deleted at
//! the P1.7 cutover) and reuses none of its
//! identifiers, so the cutover's exact-denylist CI absence test stays
//! exact.

pub(crate) mod handle;
pub(crate) mod plane;
pub(crate) mod service;
/// The Gate-B-lite custody adapter used by the durable Memory mode.
pub(crate) mod store;
pub(crate) mod types;

pub(crate) use handle::{MemoryHandle, MemoryStorage};
pub(crate) use types::{ClaimView, MemoryError, ProposeArgs, SearchArgs};
