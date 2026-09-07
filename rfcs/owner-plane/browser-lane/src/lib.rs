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
//! Family 13's §13.2 browser cell (the IndexedDB Txn subset +
//! Web Locks) is the fixture's substrate layer: every f13 vector's
//! byte inputs round-trip through real IndexedDB records (one record
//! per put, each in its OWN transaction — the journal's atomic
//! append unit mapped onto IDB's atomic unit), streams are stored
//! frame-per-record at the REAL frame boundaries [`frame_spans`]
//! reports, crash cuts are simulated at the fixture layer as
//! row-level truncation (full frames below the cut + the torn tail
//! slice) with ordered read-back equality against the in-memory
//! prefix, and the lock matrix runs over `navigator.locks` with Web
//! Workers as the other actors — the storage lane's shape
//! (`reducer --bin storage_lane`), transposed to the browser
//! substrate. Semantics then run unmodified, exactly like the CLI
//! harness.

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

/// The zone-log frame map of a hex stream, for the fixture's
/// IndexedDB substrate layer: `{header, frames: [[start, end]…],
/// durable}` per the reducer's strict walker, or `null` when the
/// stream has no valid frame structure (the fixture then falls back
/// to whole-value storage — a corrupt stream has no boundaries to
/// map).
#[wasm_bindgen]
pub fn frame_spans(stream_hex: String) -> Option<String> {
    let s = stream_hex;
    if !s.len().is_multiple_of(2)
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let bytes: Vec<u8> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex checked"))
        .collect();
    let (frames, durable) = owner_plane_reducer::edge::walk(&bytes)?;
    Some(
        serde_json::json!({
            "header": owner_plane_reducer::edge::HEADER_LEN,
            "frames": frames,
            "durable": durable,
        })
        .to_string(),
    )
}
