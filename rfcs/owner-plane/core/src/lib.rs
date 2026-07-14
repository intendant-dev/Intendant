//! owner-plane-core — the D0-A reference core.
//!
//! The fixture-minting implementation of the spec's writer side:
//! canonical CBOR (§1, E1–E10 writer side), the closed hash-domain
//! inventory (§2), suite-v1 crypto (§2.1), the deterministic vector
//! RNG + vector emitter (§13.1), the Appendix A shapes with signing
//! composition (§4.5), the §5 key schedule, and the §10.4/§10.5
//! outcome vocabulary. This crate mints the red-fixture tranche and
//! the corpus; the independent reducer that the Gate-A differential
//! harness runs against must NOT share this code.
//!
//! Spec: `../owner-plane-d0a-spec.md` (v0.5.19). Companion schema:
//! `../d0a-vector-cases.v1.json` — the drift gate in `domains::tests`
//! pins the `Tag` inventory to it, and `vector::check` enforces its
//! family vocabulary at mint time.

pub mod cbor;
pub mod corpus;
pub mod corpus_ctrl;
pub mod corpus_edge;
pub mod corpus_erase;
pub mod corpus_fold;
pub mod corpus_migration;
pub mod corpus_recovery;
pub mod corpus_status;
pub mod corpus_time;
pub mod domains;
pub mod keyschedule;
pub mod outcomes;
pub mod rng;
pub mod scenario;
pub mod shapes;
pub mod suite;
pub mod tranche;
pub mod vector;
