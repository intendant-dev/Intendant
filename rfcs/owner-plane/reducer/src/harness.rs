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
    /// The arrival-order rule: a convergence-bearing multi-item
    /// vector must list at least TWO byte-distinct delivery orders —
    /// otherwise its converge assertion degenerates to re-running
    /// one order (the fresh fold uses sorted names, which a single
    /// listed delivery may equal).
    pub convergence_ok: Result<(), String>,
    pub semantics: SemStatus,
}

impl VectorReport {
    pub fn structural_ok(&self) -> bool {
        self.container_ok.is_ok()
            && self.companion_ok.is_ok()
            && self.pairs_ok.is_ok()
            && self.decode_ok.is_ok()
            && self.convergence_ok.is_ok()
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

/// Case kinds whose semantics run the delivery-order converge
/// standard.
const CONVERGENCE_KINDS: &[&str] = &[
    "fold",
    "walkthrough",
    "journal-replay",
    "status-derive",
    "export-import",
    "audit-partition",
];

/// A convergence-bearing vector with more than one item must carry
/// at least two byte-distinct delivery orders.
fn check_convergence_orders(vector: &Json) -> Result<(), String> {
    let kind = vector["case_kind"].as_str().unwrap_or_default();
    if !CONVERGENCE_KINDS.contains(&kind) {
        return Ok(());
    }
    let n_items = vector["inputs"]["items"]
        .as_object()
        .map(|m| m.len())
        .unwrap_or(0);
    if n_items < 2 {
        return Ok(());
    }
    let deliveries = vector["inputs"]["deliveries"]
        .as_array()
        .ok_or("deliveries missing")?;
    let mut distinct: Vec<String> = deliveries.iter().map(|d| d.to_string()).collect();
    distinct.sort();
    distinct.dedup();
    if distinct.len() < 2 {
        return Err(format!(
            "{n_items} items but only {} distinct delivery order(s) — the converge \
             assertion needs at least two",
            distinct.len()
        ));
    }
    Ok(())
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
    let run = match kind {
        "fold" => run_fold_vector(vector),
        "walkthrough" => run_walkthrough(vector),
        "journal-replay" => run_journal_vector(vector),
        "export-import" => run_export_import(vector),
        "status-derive" => run_status_derive(vector),
        "audit-partition" => run_audit_partition(vector),
        "edge-admission" => crate::edge::edge_admission(vector),
        "frame-roundtrip" => crate::edge::frame_roundtrip(vector),
        "corruption-negative" => crate::edge::corruption_negative(vector),
        "crash-replay" => crate::edge::crash_replay(vector),
        "erase-crash-matrix" => crate::erase::erase_crash_matrix(vector),
        "lock-matrix" => crate::edge::lock_matrix(vector),
        _ => crate::kat::run(vector),
    };
    match run {
        Ok(status) => status,
        Err(e) => SemStatus::Fail(e),
    }
}

/// The export-import lane (D-127/D-156 construct-and-rederive): a
/// fold vector whose result additionally carries `content_digest`
/// and `release_op` — the harness re-derives BOTH from the held
/// facts (source claims → ranked leaves → root; the release item's
/// own bytes → H_op) and compares.
fn run_export_import(vector: &Json) -> Result<SemStatus, String> {
    use std::collections::BTreeMap;
    let status = run_fold_vector(vector)?;
    if status != SemStatus::Pass {
        return Ok(status);
    }
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
    let fresh_order: Vec<String> = items.keys().cloned().collect();
    let aux = parse_aux(vector)?;
    let (_, state) = match crate::fold::run_delivery_full(&items, &aux, &fresh_order) {
        Ok(v) => v,
        Err(u) => return Ok(SemStatus::Unimplemented(u.0)),
    };
    let releases = state.held_releases();
    let [(release_op, export_id, digest, sources)] = releases.as_slice() else {
        return Ok(SemStatus::Unimplemented(
            "export-import expects exactly one held release".into(),
        ));
    };
    // Re-derive the root from the LIVE sources: ranks are the sorted
    // source order (the signed set is already canonical).
    let mut leaves = Vec::new();
    for (rank, op) in sources.iter().enumerate() {
        let Some((kind, statement, sens)) = state.claim_content(op) else {
            return Ok(SemStatus::Fail(format!(
                "source {rank} is not a held claim"
            )));
        };
        let floor = match sens {
            0 => "public",
            1 => "internal",
            2 => "private",
            3 => "sensitive",
            _ => return Ok(SemStatus::Fail("source sensitivity out of range".into())),
        };
        leaves.push(crate::domains::brec_leaf(
            export_id,
            rank as u64,
            op,
            &kind,
            &statement,
            floor,
        ));
    }
    let Some(root) = crate::domains::merkle_root(&leaves) else {
        return Ok(SemStatus::Fail("empty source set".into()));
    };
    if root != *digest {
        return Ok(SemStatus::Fail(
            "re-derived root differs from the held content_digest".into(),
        ));
    }
    let want_digest = unhex(
        vector["expected"]["result"]["content_digest"]
            .as_str()
            .ok_or("result.content_digest")?,
    )?;
    let want_release = unhex(
        vector["expected"]["result"]["release_op"]
            .as_str()
            .ok_or("result.release_op")?,
    )?;
    if root.to_vec() != want_digest {
        return Ok(SemStatus::Fail(
            "re-derived root differs from the expected content_digest".into(),
        ));
    }
    if release_op.to_vec() != want_release {
        return Ok(SemStatus::Fail(
            "held release_op differs from the expected".into(),
        ));
    }
    Ok(SemStatus::Pass)
}

/// The status-derive lane (§11.2): a fold vector whose result
/// carries `derived` rows — the reducer re-derives each named
/// claim's status through its own five-step fold at `as_of_ms`.
fn run_status_derive(vector: &Json) -> Result<SemStatus, String> {
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
    let as_of = vector["inputs"]["as_of_ms"].as_u64().ok_or("as_of_ms")?;
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

    let aux = parse_aux(vector)?;
    let mut runs = Vec::new();
    for order in &deliveries {
        match crate::fold::run_delivery_full(&items, &aux, order) {
            Ok((run, _)) => runs.push(run),
            Err(u) => return Ok(SemStatus::Unimplemented(u.0)),
        }
    }
    let fresh_order: Vec<String> = items.keys().cloned().collect();
    let (fresh, state) = match crate::fold::run_delivery_full(&items, &aux, &fresh_order) {
        Ok(v) => v,
        Err(u) => return Ok(SemStatus::Unimplemented(u.0)),
    };
    for (i, run) in runs.iter().enumerate() {
        if run.final_verdicts != fresh.final_verdicts {
            return Ok(SemStatus::Fail(format!(
                "delivery {i} final state diverges from the fresh fold"
            )));
        }
    }

    let rows = vector["expected"]["result"]["derived"]
        .as_array()
        .ok_or("result.derived")?;
    for row in rows {
        let name = row["item"].as_str().ok_or("row.item")?;
        let want = row["value"].as_str().ok_or("row.value")?;
        let bytes = items.get(name).ok_or("derived names unknown item")?;
        let hash = crate::domains::h("op", bytes);
        match state.claim_status(&hash, as_of) {
            Some(got) if got == want => {}
            Some(got) => {
                return Ok(SemStatus::Fail(format!(
                    "{name}: expected status {want}, reducer derived {got}"
                )))
            }
            None => {
                return Ok(SemStatus::Fail(format!(
                    "{name}: not a held claim in the final state"
                )))
            }
        }
    }
    Ok(SemStatus::Pass)
}

/// The journal-replay lane: every listed delivery plus the fresh
/// sorted-order replay must agree on final verdicts, intervals, and
/// probes; then per_record, intervals, and state_probes compare
/// against the vector.
fn run_journal_vector(vector: &Json) -> Result<SemStatus, String> {
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
    let mut aux: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    if let Some(m) = vector["inputs"]["aux"].as_object() {
        for (name, hv) in m {
            aux.insert(name.clone(), unhex(hv.as_str().ok_or("aux not a string")?)?);
        }
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

    let mut runs = Vec::new();
    for order in &deliveries {
        match crate::journal::run_journal(&items, &aux, order) {
            Ok(run) => runs.push(run),
            Err(u) => return Ok(SemStatus::Unimplemented(u.0)),
        }
    }
    let fresh_order: Vec<String> = items.keys().cloned().collect();
    let fresh = match crate::journal::run_journal(&items, &aux, &fresh_order) {
        Ok(run) => run,
        Err(u) => return Ok(SemStatus::Unimplemented(u.0)),
    };
    for (i, run) in runs.iter().enumerate() {
        if run.final_verdicts != fresh.final_verdicts
            || run.intervals != fresh.intervals
            || run.probes != fresh.probes
        {
            return Ok(SemStatus::Fail(format!(
                "delivery {i} diverges from the fresh replay"
            )));
        }
    }
    let run = &runs[0];

    // per_record: one row per delivered record; absent pair = admits.
    let rows = vector["expected"]["result"]["per_record"]
        .as_array()
        .ok_or("per_record missing")?;
    if rows.len() != run.final_verdicts.len() {
        return Ok(SemStatus::Fail(format!(
            "per_record rows {} != delivered records {}",
            rows.len(),
            run.final_verdicts.len()
        )));
    }
    for row in rows {
        let name = row["rec"].as_str().ok_or("row.rec")?;
        let Some(verdict) = run.final_verdicts.get(name) else {
            return Ok(SemStatus::Fail(format!(
                "per_record names unknown record {name}"
            )));
        };
        let want = match (row.get("outcome"), row.get("disposition")) {
            (Some(o), Some(d)) => Some((
                o.as_str().ok_or("row.outcome")?,
                d.as_str().ok_or("row.disposition")?,
            )),
            _ => None,
        };
        if verdict.pair() != want {
            return Ok(SemStatus::Fail(format!(
                "{name}: expected {want:?}, reducer derived {:?}",
                verdict.pair()
            )));
        }
    }

    // intervals: (incarnation, terminal) exactly.
    let want_intervals: Vec<(u64, String)> = vector["expected"]["result"]["intervals"]
        .as_array()
        .ok_or("intervals missing")?
        .iter()
        .map(|iv| {
            Ok((
                iv["incarnation"].as_u64().ok_or("interval.incarnation")?,
                iv["terminal"]
                    .as_str()
                    .ok_or("interval.terminal")?
                    .to_string(),
            ))
        })
        .collect::<Result<_, &str>>()?;
    let got_intervals: Vec<(u64, String)> = run
        .intervals
        .iter()
        .map(|(i, t)| (*i, t.to_string()))
        .collect();
    if got_intervals != want_intervals {
        return Ok(SemStatus::Fail(format!(
            "intervals: expected {want_intervals:?}, got {got_intervals:?}"
        )));
    }

    // state_probes: exact-name registry, canonical-byte equality.
    if let Some(probes) = vector["expected"]["result"]["state_probes"].as_array() {
        for p in probes {
            let name = p["name"].as_str().ok_or("probe.name")?;
            let want = p["value"].as_str().ok_or("probe.value")?;
            let Some(got) = run.probes.get(name) else {
                return Ok(SemStatus::Unimplemented(format!("state probe {name:?}")));
            };
            let got_hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
            if got_hex != want {
                return Ok(SemStatus::Fail(format!(
                    "probe {name:?}: expected {want}, got {got_hex}"
                )));
            }
        }
    }

    Ok(SemStatus::Pass)
}

fn parse_aux(vector: &Json) -> Result<std::collections::BTreeMap<String, Vec<u8>>, String> {
    let mut aux = std::collections::BTreeMap::new();
    if let Some(m) = vector["inputs"]["aux"].as_object() {
        for (name, hv) in m {
            aux.insert(name.clone(), unhex(hv.as_str().ok_or("aux not a string")?)?);
        }
    }
    Ok(aux)
}

/// The audit-partition lane (§11.1/D-74): every delivered item must
/// finally ADMIT (the contract carries no per_item rows — one read
/// per vector by corpus convention), and the reducer's derived chunk
/// table must equal the expected `(index, count)` rows in index
/// order.
fn run_audit_partition(vector: &Json) -> Result<SemStatus, String> {
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
    let aux = parse_aux(vector)?;
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
    let mut runs = Vec::new();
    for order in &deliveries {
        match crate::fold::run_delivery_full(&items, &aux, order) {
            Ok((run, _)) => runs.push(run),
            Err(u) => return Ok(SemStatus::Unimplemented(u.0)),
        }
    }
    let fresh_order: Vec<String> = items.keys().cloned().collect();
    let (fresh, state) = match crate::fold::run_delivery_full(&items, &aux, &fresh_order) {
        Ok(v) => v,
        Err(u) => return Ok(SemStatus::Unimplemented(u.0)),
    };
    for (i, run) in runs.iter().enumerate() {
        if run.final_verdicts != fresh.final_verdicts {
            return Ok(SemStatus::Fail(format!(
                "delivery {i} final state diverges from the fresh fold"
            )));
        }
    }
    for (name, v) in &fresh.final_verdicts {
        if v.pair().is_some() {
            return Ok(SemStatus::Fail(format!(
                "{name}: audit-partition items must all admit, got {:?}",
                v.pair()
            )));
        }
    }
    let want: Vec<(u64, u64)> = vector["expected"]["result"]["chunks"]
        .as_array()
        .ok_or("result.chunks")?
        .iter()
        .map(|c| {
            Ok((
                c["index"].as_u64().ok_or("chunk.index")?,
                c["count"].as_u64().ok_or("chunk.count")?,
            ))
        })
        .collect::<Result<_, &str>>()?;
    let got = state.audit_chunks();
    if got != want {
        return Ok(SemStatus::Fail(format!(
            "chunks: expected {want:?}, reducer derived {got:?}"
        )));
    }
    Ok(SemStatus::Pass)
}

/// The walkthrough lane: a fold vector with REQUIRED state_probes —
/// fold semantics first, then each probe against the fresh-fold
/// final state's registry (exact names, canonical-byte equality).
fn run_walkthrough(vector: &Json) -> Result<SemStatus, String> {
    use std::collections::BTreeMap;
    let status = run_fold_vector(vector)?;
    if status != SemStatus::Pass {
        return Ok(status);
    }
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
    let aux = parse_aux(vector)?;
    let fresh_order: Vec<String> = items.keys().cloned().collect();
    let (_, state) = match crate::fold::run_delivery_full(&items, &aux, &fresh_order) {
        Ok(v) => v,
        Err(u) => return Ok(SemStatus::Unimplemented(u.0)),
    };
    let probes = vector["expected"]["result"]["state_probes"]
        .as_array()
        .ok_or("walkthrough state_probes missing")?;
    for p in probes {
        let name = p["name"].as_str().ok_or("probe.name")?;
        let want = p["value"].as_str().ok_or("probe.value")?;
        let Some(got) = state.probe(name) else {
            return Ok(SemStatus::Unimplemented(format!("state probe {name:?}")));
        };
        let got_hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
        if got_hex != want {
            return Ok(SemStatus::Fail(format!(
                "probe {name:?}: expected {want}, got {got_hex}"
            )));
        }
    }
    Ok(SemStatus::Pass)
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
    let aux = parse_aux(vector)?;
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
        match crate::fold::run_delivery_full(&items, &aux, order) {
            Ok((run, _)) => runs.push(run),
            Err(u) => return Ok(SemStatus::Unimplemented(u.0)),
        }
    }
    let fresh_order: Vec<String> = items.keys().cloned().collect();
    let fresh = match crate::fold::run_delivery_full(&items, &aux, &fresh_order) {
        Ok((run, _)) => run,
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

/// The gate predicate: a committed corpus is green only when EVERY
/// vector passes all structural layers AND semantics — a FAIL or an
/// Unimplemented committed vector is a red gate (the CLI exits
/// nonzero on it).
pub fn all_green(reports: &[VectorReport]) -> bool {
    reports
        .iter()
        .all(|r| r.structural_ok() && r.semantics == SemStatus::Pass)
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
            convergence_ok: check_convergence_orders(&v),
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

    /// Every committed vector passes container + companion schemas,
    /// pair cross-validation, the strict-decode differential, AND —
    /// since the burn-down completed — semantics. A fixture leaves
    /// `expect_pass` only by deliberate edit; a Fail anywhere is a
    /// differential finding (the erase fixture's original mint died
    /// exactly here: its release cited the flowless genesis grant,
    /// D-76 vs §11.8 — fixed by re-minting with a flow grant).
    #[test]
    fn tranche_structural_layers_green() {
        let reports = run_all(&plane_root().join("vectors")).unwrap();
        assert_eq!(
            reports.len(),
            154,
            "the corpus through the carried-head triple"
        );
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
            assert_eq!(r.semantics, SemStatus::Pass, "{}", r.file);
        }
    }

    /// The CLI gate goes red on semantic failure: a committed vector
    /// whose semantics FAIL (here: a fold vector whose per_item
    /// contradicts the reducer) makes `all_green` false — the exit
    /// path the bin maps to nonzero. Unimplemented is red too.
    #[test]
    fn semantic_red_fails_the_gate() {
        // A structurally green report set with one semantic FAIL.
        let mk = |sem: SemStatus| VectorReport {
            file: "x.json".into(),
            family: 7,
            case_kind: "fold".into(),
            container_ok: Ok(()),
            companion_ok: Ok(()),
            pairs_ok: Ok(()),
            decode_ok: Ok(()),
            convergence_ok: Ok(()),
            semantics: sem,
        };
        assert!(all_green(&[mk(SemStatus::Pass)]));
        assert!(!all_green(&[
            mk(SemStatus::Pass),
            mk(SemStatus::Fail("x".into()))
        ]));
        assert!(!all_green(&[mk(SemStatus::Unimplemented("y".into()))]));

        // End-to-end: a real vectors dir whose single vector LIES
        // about its per_item — run_all must report a semantic FAIL,
        // never Pass.
        let dir = std::env::temp_dir().join(format!("d0a-red-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = std::fs::read_to_string(
            plane_root().join("vectors/f07-delayed-reference-convergence-c1-i-c2.json"),
        )
        .unwrap();
        let mut v: Json = serde_json::from_str(&good).unwrap();
        // Flip one admitted item to a claimed failure the reducer
        // will not derive.
        let rows = v["expected"]["result"]["per_item"].as_array_mut().unwrap();
        let row = rows
            .iter_mut()
            .find(|r| r.get("outcome").is_none())
            .expect("an admitted row");
        row["outcome"] = Json::String("no-grant".into());
        row["disposition"] = Json::String("reject-permanent".into());
        std::fs::write(
            dir.join("f07-lying.json"),
            serde_json::to_string(&v).unwrap(),
        )
        .unwrap();
        let reports = run_all(&dir).unwrap();
        assert_eq!(reports.len(), 1);
        assert!(matches!(reports[0].semantics, SemStatus::Fail(_)));
        assert!(!all_green(&reports));
        std::fs::remove_dir_all(&dir).ok();
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
