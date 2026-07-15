//! The browser execution lane (§13.2 `browser` column) — the
//! reducer's lane code compiled to `wasm32-unknown-unknown`, crypto
//! routed through the [`owner_plane_reducer::crypto::Crypto`] seam
//! into WebCrypto ([`webcrypto::WebCryptoBackend`]).
//!
//! Execution shape (execution-lanes-plan lane 1): the fixture page
//! fetches the corpus manifest, calls [`run_vector`] per
//! browser-annotated vector, and publishes a report the CDP driver
//! (`driver.cjs`) polls; the driver exits nonzero unless EVERY
//! browser-annotated vector reports `semantics=PASS` with clean
//! structural layers — the same `all_green` shape the CLI harness
//! gates on.
//!
//! Honesty note on family 13: its §13.2 browser cell is the
//! IndexedDB Txn subset. Until that shim (work item 3) lands, the
//! f13 vectors here execute the reducer's engine lanes IN-MEMORY
//! inside Chromium — real wasm execution of the same lane code, but
//! NOT yet the IndexedDB substrate; the driver prints that caveat on
//! every run and the CI job name carries it.

mod webcrypto;

use owner_plane_reducer::harness::{self, SemStatus};
use wasm_bindgen::prelude::*;
use webcrypto::WebCryptoBackend;

/// Run one vector in the browser: the dep-free structural layers
/// (§10.4×§10.5 pair cross-validation, the strict-decode
/// differential, the convergence-order rule — the JSON-Schema layers
/// stay native-only) plus semantics, reported in the CLI harness's
/// row vocabulary as a JSON object. The driver's gate predicate
/// mirrors `all_green`: every structural field `"ok"` AND semantics
/// `"PASS"`, else red.
#[wasm_bindgen]
pub async fn run_vector(vector_json: String) -> Result<String, JsValue> {
    let v: serde_json::Value =
        serde_json::from_str(&vector_json).map_err(|e| JsValue::from_str(&format!("{e}")))?;
    let layer = |r: Result<(), String>| match r {
        Ok(()) => "ok".to_string(),
        Err(e) => format!("FAIL: {e}"),
    };
    let sem = match harness::run_semantics_with(&WebCryptoBackend, &v).await {
        SemStatus::Pass => "PASS".to_string(),
        SemStatus::Fail(e) => format!("FAIL: {e}"),
        SemStatus::Unimplemented(why) => format!("unimplemented ({why})"),
    };
    let row = serde_json::json!({
        "pairs": layer(harness::check_pairs(&v["expected"])),
        "decode": layer(harness::check_decode(&v)),
        "convergence": layer(harness::check_convergence_orders(&v)),
        "semantics": sem,
    });
    Ok(row.to_string())
}
