//! owner-plane-reducer — the D0-A independent reducer.
//!
//! **VENDORED** from `owner-plane-d0a` @ `583f421a` (the Gate-A stamp;
//! spec v0.5.24). Kernel semantics are STAMPED — semantic changes are
//! owner acts that land on the asset branch, never local edits here.
//! Local adaptations are confined to: workspace membership, dropped
//! bin targets (the CLI harness + per-OS storage lane stay on the
//! asset branch), and corpus path retargeting — the stamped spec,
//! companion schema, and 170-vector corpus are vendored byte-exact
//! under this crate's `corpus/` (`harness::plane_root`), with the
//! spec's SHA-256 pinned by `vendored_spec_bytes_match_the_stamp`.
//! The inline harness tests are the §D conformance gate
//! (p1-v1-profile.md): the daemon's Memory service must reproduce
//! this reducer's verdicts on the full committed corpus.
//!
//! The differential half of the Gate-A exercise: an implementation of
//! the spec's READER side (strict decoding, admission, folds, the
//! journal machine) written against the prose alone, sharing no code
//! with `owner-plane-core`. The harness runs every committed vector
//! (container + companion schema validation, §10.4/§10.5
//! cross-validation, the three-run converge standard) against this
//! reducer; a divergence between the reducer's result and a vector's
//! expectation is a finding — in the fixture, the reducer, or the
//! prose — and feeds the Gate-A discrepancy audit.
//!
//! Spec: `corpus/owner-plane-d0a-spec.md` (v0.5.24, the stamped cut).

pub mod cbor;
pub mod crypto;
pub mod domains;
pub mod edge;
pub mod envelope;
pub mod erase;
pub mod fold;
pub mod harness;
pub mod journal;
pub mod kat;
pub mod outcomes;
pub mod policies;
