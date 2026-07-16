//! The P1 Memory service — controller-owned, D0-A-format, ephemeral.
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
//! **Write bar (ratified — the P1 kickoff brief):** this build is
//! EPHEMERAL-ONLY. The op log lives in memory and dies with the
//! daemon; every view is labeled `durability: "ephemeral"`. Durable
//! local writes unlock only after the Gate-B-lite custody subset, the
//! P0.5 checkpoint replacement, and the tombed-memory cutover — in
//! that program order. Do not add persistence here ahead of it.
//!
//! This module is deliberately unrelated to the legacy
//! `store_memory`/`recall_memory`/`.intendant/memory.json` system
//! (tombed; cutover is a later P1 slice) and reuses none of its
//! identifiers, so the cutover's exact-denylist CI absence test stays
//! exact while the two coexist.

pub(crate) mod handle;
pub(crate) mod plane;
pub(crate) mod service;
pub(crate) mod types;

pub(crate) use handle::MemoryHandle;
pub(crate) use types::{ClaimView, MemoryError, ProposeArgs, SearchArgs};
