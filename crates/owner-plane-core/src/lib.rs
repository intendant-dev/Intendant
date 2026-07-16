//! owner-plane-core — the D0-A reference core (vendored subset).
//!
//! **VENDORED** from `owner-plane-d0a` @ `583f421a` (the Gate-A stamp;
//! spec v0.5.24). Kernel semantics are STAMPED — semantic changes are
//! owner acts that land on the asset branch, never local edits here.
//! Local adaptations are confined to: workspace membership, corpus
//! path retargeting (the stamped spec + companion live under
//! `../owner-plane-reducer/corpus/`), and trimming to the writer-side
//! modules the daemon consumes — the fixture-minting machinery
//! (`corpus_*`, `tranche`, `surfaces`, `vector`, `coverage`, `rng`,
//! `bin/mint`) stays on the asset branch. `scenario` (the Appendix B
//! pinned-policy prelude: B.2/B.3 policy literals + the genesis zone
//! policy) is kept — services mint under those pinned policies.
//!
//! What remains is the spec's writer side: canonical CBOR (§1, E1–E10
//! writer side), the closed hash-domain inventory (§2), suite-v1
//! crypto (§2.1), the Appendix A shapes with signing composition
//! (§4.5), the §5 key schedule, and the §10.4/§10.5 outcome
//! vocabulary. The independent reducer that the differential harness
//! runs against must NOT share this code.

pub mod cbor;
pub mod domains;
pub mod keyschedule;
pub mod outcomes;
pub mod scenario;
pub mod shapes;
pub mod suite;
