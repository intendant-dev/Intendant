//! owner-plane-reducer — the D0-A independent reducer.
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
//! Spec: `../owner-plane-d0a-spec.md` (v0.5.19).

pub mod cbor;
pub mod domains;
pub mod edge;
pub mod envelope;
pub mod fold;
pub mod harness;
pub mod journal;
pub mod kat;
pub mod outcomes;
