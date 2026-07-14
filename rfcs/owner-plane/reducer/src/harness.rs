//! The differential harness (§13.1 + the program order).
//!
//! For every committed vector under `../vectors/`:
//!
//! 1. **Container schema** — the §13.1 JSON Schema, extracted from
//!    the spec's own fenced block and compiled with a real Draft
//!    2020-12 engine.
//! 2. **Companion schema** — `../d0a-vector-cases.v1.json` (family
//!    vocabularies + per-case_kind contracts), same engine. A vector
//!    failing either is invalid — a harness never invents family
//!    semantics.
//! 3. **§10.4×§10.5 cross-validation** — every (outcome, disposition)
//!    pair anywhere under `expected`, against the reducer's own
//!    table.
//! 4. **Strict-decode differential** — every `inputs.items` and
//!    `inputs.aux` byte string must pass the reducer's strict reader
//!    (the reference core mints canonical bytes; a decode failure on
//!    either side is a finding). Deliberately shape-invalid fixture
//!    records are CDDL-level, not CBOR-level, so this holds for the
//!    whole tranche.
//! 5. **Semantics** — the fold/journal dispatch. Unimplemented cases
//!    report as such: the tranche stays red until the reducer's
//!    engine covers it, which is the program's definition of done.

use serde_json::Value as Json;
use std::path::{Path, PathBuf};

use crate::cbor;
use crate::outcomes;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemStatus {
    /// The engine met an operation type or lane outside its coverage
    /// — the honest red state; the reason names the frontier.
    Unimplemented(String),
    Pass,
    Fail(String),
}

#[derive(Debug)]
pub struct VectorReport {
    pub file: String,
    pub family: u64,
    pub case_kind: String,
    pub container_ok: Result<(), String>,
    pub companion_ok: Result<(), String>,
    pub pairs_ok: Result<(), String>,
    pub decode_ok: Result<(), String>,
    pub semantics: SemStatus,
}

impl VectorReport {
    pub fn structural_ok(&self) -> bool {
        self.container_ok.is_ok()
            && self.companion_ok.is_ok()
            && self.pairs_ok.is_ok()
            && self.decode_ok.is_ok()
    }
}

/// The `rfcs/owner-plane/` root (one above this crate).
pub fn plane_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Extract the §13.1 container schema from the spec's fenced block.
pub fn container_schema(spec_text: &str) -> Result<Json, String> {
    let anchor = "### 13.1 Vector file schema";
    let at = spec_text.find(anchor).ok_or("missing §13.1 anchor")?;
    let rest = &spec_text[at..];
    let open = rest.find("```json").ok_or("missing ```json fence")?;
    let body = &rest[open + 7..];
    let close = body.find("```").ok_or("missing closing fence")?;
    serde_json::from_str(body[..close].trim()).map_err(|e| format!("container parse: {e}"))
}

fn validate(schema: &Json, instance: &Json) -> Result<(), String> {
    let validator = jsonschema::validator_for(schema).map_err(|e| format!("compile: {e}"))?;
    let errors: Vec<String> = validator
        .iter_errors(instance)
        .map(|e| format!("{} @ {}", e, e.instance_path))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// §13.1: outcome/disposition stay plain strings in the schemas; the
/// harness cross-validates every pair under `expected`.
fn check_pairs(expected: &Json) -> Result<(), String> {
    fn walk(node: &Json, path: &str, bad: &mut Vec<String>) {
        match node {
            Json::Object(m) => {
                match (m.get("outcome"), m.get("disposition")) {
                    (None, None) => {}
                    (Some(o), Some(d)) => {
                        let (Some(o), Some(d)) = (o.as_str(), d.as_str()) else {
                            bad.push(format!("{path}: non-string pair"));
                            return;
                        };
                        if !outcomes::valid_pair(o, d) {
                            bad.push(format!("{path}: illegal pair ({o}, {d})"));
                        }
                    }
                    _ => bad.push(format!("{path}: half a pair")),
                }
                for (k, v) in m {
                    walk(v, &format!("{path}.{k}"), bad);
                }
            }
            Json::Array(a) => {
                for (i, v) in a.iter().enumerate() {
                    walk(v, &format!("{path}[{i}]"), bad);
                }
            }
            _ => {}
        }
    }
    let mut bad = Vec::new();
    walk(expected, "expected", &mut bad);
    if bad.is_empty() {
        Ok(())
    } else {
        Err(bad.join("; "))
    }
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2)
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("not lowercase even-length hex".into());
    }
    Ok((0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect())
}

/// Every named byte string in `inputs.items` and `inputs.aux` decodes
/// under the strict reader.
fn check_decode(vector: &Json) -> Result<(), String> {
    let mut bad = Vec::new();
    for field in ["items", "aux"] {
        if let Some(m) = vector["inputs"][field].as_object() {
            for (name, hexv) in m {
                let Some(hexs) = hexv.as_str() else {
                    bad.push(format!("{field}.{name}: not a string"));
                    continue;
                };
                match unhex(hexs) {
                    Ok(bytes) => {
                        if let Err(e) = cbor::decode(&bytes) {
                            bad.push(format!("{field}.{name}: {e:?}"));
                        }
                    }
                    Err(e) => bad.push(format!("{field}.{name}: {e}")),
                }
            }
        }
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(bad.join("; "))
    }
}

/// The semantic dispatch — grows with the reducer's engine.
///
/// `fold` vectors run the three-run standard: EVERY listed delivery
/// order PLUS a fresh fold of the union (sorted item names — the
/// engine's fixpoint re-evaluation makes arrival order immaterial,
/// which is exactly what the standard asserts), identical final
/// state required, then per_item and trace comparison.
fn run_semantics(vector: &Json) -> SemStatus {
    let kind = vector["case_kind"].as_str().unwrap_or_default();
    if kind != "fold" {
        return SemStatus::Unimplemented(format!("case_kind {kind}"));
    }
    match run_fold_vector(vector) {
        Ok(status) => status,
        Err(e) => SemStatus::Fail(e),
    }
}

fn run_fold_vector(vector: &Json) -> Result<SemStatus, String> {
    use std::collections::BTreeMap;

    let mut items: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (name, hv) in vector["inputs"]["items"]
        .as_object()
        .ok_or("items missing")?
    {
        items.insert(
            name.clone(),
            unhex(hv.as_str().ok_or("item not a string")?)?,
        );
    }
    let deliveries: Vec<Vec<String>> = vector["inputs"]["deliveries"]
        .as_array()
        .ok_or("deliveries missing")?
        .iter()
        .map(|d| {
            d.as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(str::to_string))
                        .collect()
                })
                .ok_or("delivery not an array")
        })
        .collect::<Result<_, _>>()?;

    // The three-run standard.
    let mut runs = Vec::new();
    for order in &deliveries {
        match crate::fold::run_delivery(&items, order) {
            Ok(run) => runs.push(run),
            Err(u) => return Ok(SemStatus::Unimplemented(u.0)),
        }
    }
    let fresh_order: Vec<String> = items.keys().cloned().collect();
    let fresh = match crate::fold::run_delivery(&items, &fresh_order) {
        Ok(run) => run,
        Err(u) => return Ok(SemStatus::Unimplemented(u.0)),
    };
    for (i, run) in runs.iter().enumerate() {
        if run.final_verdicts != fresh.final_verdicts {
            return Ok(SemStatus::Fail(format!(
                "delivery {i} final state diverges from the fresh fold"
            )));
        }
    }

    // per_item: exactly one row per delivered item; absent pair =
    // finally admits.
    let expected_rows = vector["expected"]["result"]["per_item"]
        .as_array()
        .ok_or("per_item missing")?;
    let final_v = &fresh.final_verdicts;
    if expected_rows.len() != final_v.len() {
        return Ok(SemStatus::Fail(format!(
            "per_item rows {} != delivered items {}",
            expected_rows.len(),
            final_v.len()
        )));
    }
    for row in expected_rows {
        let name = row["item"].as_str().ok_or("row.item")?;
        let Some(verdict) = final_v.get(name) else {
            return Ok(SemStatus::Fail(format!(
                "per_item names unknown item {name}"
            )));
        };
        let want = match (row.get("outcome"), row.get("disposition")) {
            (Some(o), Some(d)) => Some((
                o.as_str().ok_or("row.outcome")?,
                d.as_str().ok_or("row.disposition")?,
            )),
            _ => None,
        };
        let got = verdict.pair();
        if got != want {
            return Ok(SemStatus::Fail(format!(
                "{name}: expected {want:?}, reducer derived {got:?}"
            )));
        }
    }

    // trace: in delivery #d, immediately after `after` folds, `item`
    // holds (outcome, disposition).
    if let Some(trace) = vector["expected"]["result"]["trace"].as_array() {
        for t in trace {
            let d = t["delivery"].as_u64().ok_or("trace.delivery")? as usize;
            let after = t["after"].as_str().ok_or("trace.after")?;
            let item = t["item"].as_str().ok_or("trace.item")?;
            let want = (
                t["outcome"].as_str().ok_or("trace.outcome")?,
                t["disposition"].as_str().ok_or("trace.disposition")?,
            );
            let run = runs.get(d).ok_or("trace delivery index")?;
            let pos = deliveries[d]
                .iter()
                .position(|n| n == after)
                .ok_or("trace.after not in delivery")?;
            let snap = run.snapshots.get(pos).ok_or("trace snapshot")?;
            let got = snap.get(item).and_then(|v| v.pair());
            if got != Some(want) {
                return Ok(SemStatus::Fail(format!(
                    "trace d{d} after {after}: {item} expected {want:?}, got {got:?}"
                )));
            }
        }
    }

    Ok(SemStatus::Pass)
}

/// Run the full harness over a vectors directory.
pub fn run_all(vectors_dir: &Path) -> Result<Vec<VectorReport>, String> {
    let spec = std::fs::read_to_string(plane_root().join("owner-plane-d0a-spec.md"))
        .map_err(|e| format!("spec: {e}"))?;
    let container = container_schema(&spec)?;
    let companion: Json = serde_json::from_str(
        &std::fs::read_to_string(plane_root().join("d0a-vector-cases.v1.json"))
            .map_err(|e| format!("companion: {e}"))?,
    )
    .map_err(|e| format!("companion parse: {e}"))?;

    let mut files: Vec<PathBuf> = std::fs::read_dir(vectors_dir)
        .map_err(|e| format!("vectors dir: {e}"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();

    let mut reports = Vec::new();
    for path in files {
        let v: Json = serde_json::from_str(
            &std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?,
        )
        .map_err(|e| format!("{}: {e}", path.display()))?;
        reports.push(VectorReport {
            file: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
            family: v["family"].as_u64().unwrap_or(0),
            case_kind: v["case_kind"].as_str().unwrap_or_default().to_string(),
            container_ok: validate(&container, &v),
            companion_ok: validate(&companion, &v),
            pairs_ok: check_pairs(&v["expected"]),
            decode_ok: check_decode(&v),
            semantics: run_semantics(&v),
        });
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_schema_extracts_and_compiles() {
        let spec = std::fs::read_to_string(plane_root().join("owner-plane-d0a-spec.md")).unwrap();
        let schema = container_schema(&spec).unwrap();
        assert_eq!(
            schema["$id"],
            "https://intendant.dev/schemas/d0a-vector.v1.json"
        );
        jsonschema::validator_for(&schema).unwrap();
    }

    #[test]
    fn companion_compiles() {
        let companion: Json = serde_json::from_str(
            &std::fs::read_to_string(plane_root().join("d0a-vector-cases.v1.json")).unwrap(),
        )
        .unwrap();
        jsonschema::validator_for(&companion).unwrap();
    }

    /// The tranche's structural layers: every committed vector passes
    /// container + companion schemas, pair cross-validation, and the
    /// strict-decode differential. Semantics stay red until the
    /// reducer's engine lands — asserted as exactly Unimplemented so
    /// engine progress must flip this test deliberately.
    #[test]
    fn tranche_structural_layers_green() {
        let reports = run_all(&plane_root().join("vectors")).unwrap();
        assert_eq!(reports.len(), 8, "the opening tranche is eight vectors");
        // The burn-down: fixtures flip to Pass as engine coverage
        // grows; a Fail anywhere is a differential finding.
        let expect_pass = [
            "f07-delayed-reference-convergence-c1-i-c2.json",
            "f07-negation-residual-acceptance.json",
            "f07-pending-revocation-window-grant-completing-rotation.json",
        ];
        for r in &reports {
            assert!(
                r.structural_ok(),
                "{}: container={:?} companion={:?} pairs={:?} decode={:?}",
                r.file,
                r.container_ok,
                r.companion_ok,
                r.pairs_ok,
                r.decode_ok
            );
            if expect_pass.contains(&r.file.as_str()) {
                assert_eq!(r.semantics, SemStatus::Pass, "{}", r.file);
            } else {
                assert!(
                    matches!(r.semantics, SemStatus::Unimplemented(_)),
                    "{}: {:?}",
                    r.file,
                    r.semantics
                );
            }
        }
    }

    /// Negative controls: the harness actually rejects bad inputs.
    #[test]
    fn harness_rejects_bad_vectors() {
        let spec = std::fs::read_to_string(plane_root().join("owner-plane-d0a-spec.md")).unwrap();
        let container = container_schema(&spec).unwrap();

        // Missing required key.
        let v: Json = serde_json::json!({ "family": 1, "name": "x" });
        assert!(validate(&container, &v).is_err());

        // Family out of range.
        let v: Json = serde_json::json!({
            "family": 15, "name": "x", "case_kind": "fold", "source": "1",
            "surfaces": ["core"], "inputs": {}, "expected": { "result": 1 },
        });
        assert!(validate(&container, &v).is_err());

        // Illegal pair.
        assert!(check_pairs(&serde_json::json!({
            "outcome": "malformed", "disposition": "pending-dependency"
        }))
        .is_err());
        // Half a pair.
        assert!(check_pairs(&serde_json::json!({ "outcome": "malformed" })).is_err());
        // Legal pair nested in a result row.
        assert!(check_pairs(&serde_json::json!({
            "result": { "per_item": [
                { "item": "a" },
                { "item": "b", "outcome": "cutoff", "disposition": "quarantine-reproposal" },
            ]}
        }))
        .is_ok());

        // Non-canonical bytes fail the decode layer.
        let v: Json = serde_json::json!({
            "inputs": { "items": { "x": "1800" } }
        });
        assert!(check_decode(&v).is_err());
        let v: Json = serde_json::json!({
            "inputs": { "items": { "x": "a2616201616102" } }
        });
        assert!(check_decode(&v).is_err());
    }
}
