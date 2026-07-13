//! owner-plane-core — the D0-A reference core.
//!
//! The fixture-minting implementation of the spec's writer-side
//! primitives: canonical CBOR (§1, E1–E10 writer side), the closed
//! hash-domain inventory (§2), and the deterministic vector RNG
//! (§13.1). This crate mints the red-fixture tranche and the corpus;
//! the independent reducer that the Gate-A differential harness runs
//! against must NOT share this code.
//!
//! Spec: `../owner-plane-d0a-spec.md` (v0.5.19). Companion schema:
//! `../d0a-vector-cases.v1.json` — the drift gate in `domains::tests`
//! pins the `Tag` inventory to it.

pub mod cbor;
pub mod domains;
pub mod rng;
pub mod shapes;
pub mod suite;
