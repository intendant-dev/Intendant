//! The browser execution lane (§13.2 `browser` column) — the
//! reducer's lane code compiled to `wasm32-unknown-unknown`, crypto
//! routed through the [`owner_plane_reducer::crypto::Crypto`] seam.
//!
//! SCAFFOLD STATUS (execution-lanes-plan lane 1): this crate
//! currently pins the wasm dependency story (schema-less reducer,
//! `getrandom/js` unification) and exposes the per-vector entry
//! point. The WebCrypto backend is DELIBERATELY unwired — every
//! primitive returns a "not wired" error, so a browser-annotated
//! crypto vector reports FAIL, never a false PASS; engine-lane
//! vectors (whose §13.2 browser requirement is the IndexedDB Txn
//! subset, not WebCrypto) already execute for real. Work items 2–3
//! (the SubtleCrypto backend, the fixture page + CDP driver, the
//! IndexedDB Txn shim) land next; the advisory CI job arrives only
//! WITH the driver, so no green can be misread before the lane
//! actually runs.

use owner_plane_reducer::crypto::Crypto;
use owner_plane_reducer::harness::{self, SemStatus};
use wasm_bindgen::prelude::*;

/// The WebCrypto (SubtleCrypto) backend — unwired scaffold. Each
/// method will await the corresponding `crypto.subtle` call
/// (`digest`, `importKey`+`verify`, ECDH `deriveBits` composed into
/// HPKE, `encrypt`/`decrypt`, HKDF/PBKDF2 `deriveBits`).
struct WebCryptoBackend;

const UNWIRED: &str = "WebCrypto backend not wired yet (browser-lane scaffold)";

impl Crypto for WebCryptoBackend {
    async fn sha256(&self, _data: &[u8]) -> Result<[u8; 32], String> {
        Err(UNWIRED.into())
    }
    async fn ed25519_verify(&self, _pk: &[u8], _msg: &[u8], _sig: &[u8]) -> Result<bool, String> {
        Err(UNWIRED.into())
    }
    async fn p256_verify(&self, _pk: &[u8], _msg: &[u8], _sig: &[u8]) -> Result<bool, String> {
        Err(UNWIRED.into())
    }
    async fn p256_point_valid(&self, _sec1: &[u8]) -> Result<bool, String> {
        Err(UNWIRED.into())
    }
    async fn p256_pk_of(&self, _sk: &[u8; 32]) -> Result<Option<[u8; 65]>, String> {
        Err(UNWIRED.into())
    }
    async fn hpke_open(
        &self,
        _sk: &[u8; 32],
        _enc: &[u8],
        _info: &[u8],
        _aad: &[u8],
        _ct: &[u8],
    ) -> Result<Option<Vec<u8>>, String> {
        Err(UNWIRED.into())
    }
    async fn aes_gcm_seal(
        &self,
        _key: &[u8; 32],
        _nonce: &[u8; 12],
        _aad: &[u8],
        _pt: &[u8],
    ) -> Result<Vec<u8>, String> {
        Err(UNWIRED.into())
    }
    async fn aes_gcm_open(
        &self,
        _key: &[u8; 32],
        _nonce: &[u8; 12],
        _aad: &[u8],
        _ct: &[u8],
    ) -> Result<Option<Vec<u8>>, String> {
        Err(UNWIRED.into())
    }
    async fn hkdf_sha256(
        &self,
        _salt: &[u8],
        _ikm: &[u8],
        _info: &[u8],
        _out_len: usize,
    ) -> Result<Vec<u8>, String> {
        Err(UNWIRED.into())
    }
    async fn pbkdf2_hmac_sha512(
        &self,
        _password: &[u8],
        _salt: &[u8],
        _iterations: u32,
    ) -> Result<[u8; 64], String> {
        Err(UNWIRED.into())
    }
    async fn ed25519_pk_of_seed(&self, _seed: &[u8; 32]) -> Result<[u8; 32], String> {
        Err(UNWIRED.into())
    }
}

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
