//! Corpus family 11 audit partition (§11.1 `m.audit`, D-74/D-83):
//! service-actor rows attested by the writing device's own
//! certificate on the audit space, one read = one `read_id` with
//! chunk indexes exactly `0..count−1`, disjoint result sets, one
//! zone per read.
//!
//! Lane conventions (register entries): an `audit-partition` vector's
//! items must ALL finally admit (the contract carries no per_item
//! rows); its `chunks` = the admitted audit rows' `(index, count)`
//! pairs in index order, and the corpus keeps one read per vector.
//! Negatives ride `fold` vectors (per_item pairs available). The
//! audit rows ride dev1's own lineage chain (the genesis audit grant
//! shares it), so claims and rows sequence together.

use crate::shapes::envelope::Signedop;
use crate::shapes::memory::{Auditprin, Maudit};
use crate::tranche::{draw_id, items, PlaneRig, T0_MS};
use crate::vector::{Expected, Vector};
use serde_json::{json, Map as JsonMap, Value as Json};

fn audit_vector(
    name: &str,
    case_kind: &str,
    rig: PlaneRig,
    item_list: &[(&str, &Signedop)],
    result: Json,
) -> Vector {
    let mut inputs = JsonMap::new();
    inputs.insert("items".into(), items(item_list));
    let forward: Vec<&str> = item_list.iter().map(|(n, _)| *n).collect();
    let mut reversed = forward.clone();
    reversed.reverse();
    inputs.insert("deliveries".into(), json!([forward, reversed]));
    Vector {
        family: 11,
        name: name.into(),
        case_kind: case_kind.into(),
        source: "11.1".into(),
        surfaces: vec!["core".into()],
        rng: Some(rig.rng.into_json()),
        inputs,
        expected: Expected::Result(result),
    }
}

fn row_body(
    rig: &mut PlaneRig,
    read_id: [u8; 16],
    index: u64,
    count: u64,
    result_ids: Vec<[u8; 32]>,
) -> Maudit {
    let (zone, home) = (rig.zone_id, rig.home_space);
    Maudit {
        principal: Auditprin::Device {
            device: rig.dev1.device_id,
        },
        read_id,
        chunk_index: index,
        chunk_count: count,
        scope_zone: zone,
        scope_spaces: vec![home],
        result_ids,
        at_ms: T0_MS + 3_600_000,
    }
}

/// Two claims, one read, two chunks — the partition invariants hold
/// and the harness re-derives the chunk table.
pub fn f11_audit_partition_two_chunks() -> Vector {
    let name = "f11-audit-two-chunks";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let g1 = rig.genesis_grant.clone();
    let ga = rig.genesis_audit_grant.clone();
    let i1 = rig.claim(
        &d1,
        &g1,
        "i1",
        "the mooring chart names two berths",
        1,
        None,
    );
    let i2 = rig.claim(
        &d1,
        &g1,
        "i2",
        "the second berth floods at spring tide",
        2,
        Some(i1.op_hash()),
    );
    let read_id = draw_id(&mut rig.rng, "audit.read_id");
    let b0 = row_body(&mut rig, read_id, 0, 2, vec![i1.op_hash()]);
    let a0 = rig.audit_row(&d1, &ga, "a0", b0, 3, Some(i2.op_hash()));
    let b1 = row_body(&mut rig, read_id, 1, 2, vec![i2.op_hash()]);
    let a1 = rig.audit_row(&d1, &ga, "a1", b1, 4, Some(a0.op_hash()));
    let c1 = rig.genesis_op.clone();
    audit_vector(
        "audit-partition-two-chunks",
        "audit-partition",
        rig,
        &[
            ("c1", &c1),
            ("i1", &i1),
            ("i2", &i2),
            ("a0", &a0),
            ("a1", &a1),
        ],
        json!({
            "chunks": [ { "index": 0, "count": 2 }, { "index": 1, "count": 2 } ],
            "converge": true,
        }),
    )
}

/// A zero-result audited read writes exactly one empty chunk
/// `{0, 1}` (D-83).
pub fn f11_audit_zero_result_single_chunk() -> Vector {
    let name = "f11-audit-zero-result";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let ga = rig.genesis_audit_grant.clone();
    let read_id = draw_id(&mut rig.rng, "audit.read_id");
    let b0 = row_body(&mut rig, read_id, 0, 1, vec![]);
    let a0 = rig.audit_row(&d1, &ga, "a0", b0, 1, None);
    let c1 = rig.genesis_op.clone();
    audit_vector(
        "audit-zero-result-single-chunk",
        "audit-partition",
        rig,
        &[("c1", &c1), ("a0", &a0)],
        json!({
            "chunks": [ { "index": 0, "count": 1 } ],
            "converge": true,
        }),
    )
}

/// A chunk index at or beyond `count` violates `0..count−1`:
/// `(body-invariant, reject-permanent)`.
pub fn f11_audit_chunk_index_out_of_range() -> Vector {
    let name = "f11-audit-index-range";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let ga = rig.genesis_audit_grant.clone();
    let read_id = draw_id(&mut rig.rng, "audit.read_id");
    let bx = row_body(&mut rig, read_id, 2, 2, vec![]);
    let ax = rig.audit_row(&d1, &ga, "ax", bx, 1, None);
    let c1 = rig.genesis_op.clone();
    audit_vector(
        "audit-chunk-index-out-of-range",
        "fold",
        rig,
        &[("c1", &c1), ("ax", &ax)],
        json!({
            "per_item": [
                { "item": "ax", "outcome": "body-invariant", "disposition": "reject-permanent" },
                { "item": "c1" },
            ],
            "converge": true,
        }),
    )
}

/// One D-74 partition-conflict negative: a base row admits, the
/// conflicting row `(body-invariant, reject-permanent)` — the chain
/// orders the pair, so arrival order cannot flip the winner.
fn partition_conflict(
    name: &'static str,
    vector_name: &'static str,
    build_conflict: impl FnOnce(&mut PlaneRig, [u8; 16], [u8; 32], [u8; 32]) -> Maudit,
) -> Vector {
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let g1 = rig.genesis_grant.clone();
    let ga = rig.genesis_audit_grant.clone();
    let i1 = rig.claim(&d1, &g1, "i1", "the base row audits this claim", 1, None);
    let read_id = draw_id(&mut rig.rng, "audit.read_id");
    let b0 = row_body(&mut rig, read_id, 0, 2, vec![i1.op_hash()]);
    let a0 = rig.audit_row(&d1, &ga, "a0", b0, 2, Some(i1.op_hash()));
    let bx = build_conflict(&mut rig, read_id, i1.op_hash(), a0.op_hash());
    let ax = rig.audit_row(&d1, &ga, "ax", bx, 3, Some(a0.op_hash()));
    let c1 = rig.genesis_op.clone();
    audit_vector(
        vector_name,
        "fold",
        rig,
        &[("c1", &c1), ("i1", &i1), ("a0", &a0), ("ax", &ax)],
        json!({
            "per_item": [
                { "item": "a0" },
                { "item": "ax", "outcome": "body-invariant", "disposition": "reject-permanent" },
                { "item": "c1" },
                { "item": "i1" },
            ],
            "converge": true,
        }),
    )
}

/// D-74 negatives: each partition invariant violated once.
pub fn f11_audit_conflicts() -> Vec<Vector> {
    vec![
        partition_conflict(
            "f11-audit-dup-index",
            "audit-duplicate-chunk-index",
            |rig, read_id, _i1, _| {
                // index 0 again (empty result set — only the index
                // collides).
                row_body(rig, read_id, 0, 2, vec![])
            },
        ),
        partition_conflict(
            "f11-audit-principal",
            "audit-changed-principal",
            |rig, read_id, _i1, _| {
                let mut b = row_body(rig, read_id, 1, 2, vec![]);
                b.principal = Auditprin::DeviceSession {
                    device: rig.dev1.device_id,
                    session: "s-1".into(),
                };
                b
            },
        ),
        partition_conflict(
            "f11-audit-scope",
            "audit-changed-scope",
            |rig, read_id, _i1, _| {
                let mut b = row_body(rig, read_id, 1, 2, vec![]);
                b.scope_spaces = vec![rig.home_space, rig.audit_space];
                b
            },
        ),
        partition_conflict(
            "f11-audit-count",
            "audit-changed-count",
            |rig, read_id, _i1, _| row_body(rig, read_id, 1, 3, vec![]),
        ),
        partition_conflict(
            "f11-audit-overlap",
            "audit-overlapping-result-sets",
            |rig, read_id, i1, _| row_body(rig, read_id, 1, 2, vec![i1]),
        ),
    ]
}

pub fn corpus_audit() -> Vec<Vector> {
    let mut out = vec![
        f11_audit_partition_two_chunks(),
        f11_audit_zero_result_single_chunk(),
        f11_audit_chunk_index_out_of_range(),
    ];
    out.extend(f11_audit_conflicts());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_audit_corpus_matches_builders() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vectors");
        for v in corpus_audit() {
            let path = dir.join(format!("f{:02}-{}.json", v.family, v.name));
            let committed = std::fs::read_to_string(&path)
                .unwrap_or_else(|_| panic!("{} not minted", path.display()));
            assert_eq!(
                committed,
                v.to_file_string(),
                "{} drifted from its builder",
                v.name
            );
        }
    }

    #[test]
    fn audit_corpus_checks_clean() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let companion: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("d0a-vector-cases.v1.json")).unwrap(),
        )
        .unwrap();
        for v in corpus_audit() {
            crate::vector::check(&v.to_json(), &companion)
                .unwrap_or_else(|e| panic!("{}: {e}", v.name));
        }
    }
}
