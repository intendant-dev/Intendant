//! Corpus family 14 (§12 migration/projection): M1 re-encapsulation
//! byte-equality, release-stamp projection completeness, and the
//! umbrella App C #2 offline-confirmation fixture.
//!
//! Fixture conventions (register entries):
//! - `reencapsulation` re-seals `p1` (exact SignedOperation bytes)
//!   under fixed KAT parameters — DEK `[0x91;32]`, nonce `[0x92;12]`,
//!   the family-5 plane/zone constants — and opens it back; the M1
//!   contract is byte equality of the recovered operation.
//! - `projection` extracts the release's complete classification
//!   evaluation point `{data_frontier, control_frontier, as_of_ms}`
//!   (§11.7's stamp); a release body missing a stamp component is
//!   `body-invariant` (signed properly, so the body-hash stage
//!   passes and the CDDL gap is what fires).
//! - `offline-confirmation` documents umbrella App C #2; the run has
//!   NOT been performed — `recorded` carries the §4.5 working
//!   position verbatim intent, and the Gate-A audit tracks the open
//!   confirmation (the §13.3 text expects the result recorded in
//!   §15 WHEN performed).

use crate::cbor;
use crate::shapes::envelope::{seal_op, OpSigner};
use crate::shapes::identity::{Endpoint, Flow};
use crate::shapes::memory::{merkle_root, Bundleleaf, Bundlerec};
use crate::shapes::{Class, Kind, Verb};
use crate::tranche::{draw_id, PlaneRig, T0_MS};
use crate::vector::{Expected, Vector};
use serde_json::{json, Map as JsonMap};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A rig with one claim and one flow-matched release; returns
/// (claim bytes, release op, stamp fields).
struct ReleaseFixture {
    claim_bytes: Vec<u8>,
    release: crate::shapes::envelope::Signedop,
}

fn release_fixture(name: &str) -> ReleaseFixture {
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let g1 = rig.genesis_grant.clone();
    let (gz, home) = (rig.zone_id, rig.home_space);
    let dest_zone = draw_id(&mut rig.rng, "dest.zone_id");
    let dest_space = draw_id(&mut rig.rng, "dest.space_id");
    let mut gf_grant = rig.grant_in(
        "grantflow",
        &d1,
        vec![Verb::Read, Verb::Export],
        gz,
        vec![home],
    );
    gf_grant.flows = Some(vec![Flow {
        from_zone: gz,
        from_space: None,
        to: Endpoint::Plane {
            plane_id: rig.plane_id,
            zone_id: dest_zone,
            space_id: dest_space,
        },
        kinds: None,
        class_ceiling: Class::Sensitive,
        expiry_deadline_ms: T0_MS + 10 * 86_400_000,
    }]);
    let _gf = rig.grant_op(gf_grant.clone());
    const STMT: &str = "pier decking replaced along the north span";
    let i1 = rig.claim(&d1, &g1, "i1", STMT, 1, None);
    let export_id = draw_id(&mut rig.rng, "rel.export_id");
    let digest = merkle_root(&[Bundleleaf {
        export_id,
        rec_index: 0,
        rec: Bundlerec {
            op: i1.op_hash(),
            kind: Kind::Observation,
            statement: STMT.into(),
            class_floor: Class::Private,
        },
    }
    .leaf_hash()]);
    let data_frontier = rig.rng.draw32("rel.data_frontier");
    let release = rig.release_op_signed(
        &d1,
        &gf_grant,
        "rel",
        export_id,
        vec![i1.op_hash()],
        digest,
        dest_zone,
        dest_space,
        data_frontier,
        2,
        Some(i1.op_hash()),
    );
    ReleaseFixture {
        claim_bytes: i1.encode(),
        release,
    }
}

pub fn corpus_migration() -> Vec<Vector> {
    let fx = release_fixture("f14-migration-fixture");
    let mut out = Vec::new();

    // M1 re-encapsulation: exact operation bytes survive the P1→P2
    // container move.
    let mut inputs = JsonMap::new();
    inputs.insert("p1".into(), json!(hex(&fx.claim_bytes)));
    out.push(Vector {
        family: 14,
        name: "reencapsulation-byte-equality".into(),
        case_kind: "reencapsulation".into(),
        source: "12".into(),
        surfaces: vec![
            "core".into(),
            "storage-macos".into(),
            "storage-linux".into(),
            "storage-windows".into(),
        ],
        rng: None,
        inputs,
        expected: Expected::Result(json!({ "identical": true })),
    });

    // Stamp projection: the complete classification evaluation point.
    let body = &fx.release.body;
    let stamp = |key: &str| -> Vec<u8> {
        let cbor::Value::Map(entries) = body else {
            panic!("release body is a map")
        };
        entries
            .iter()
            .find_map(|(k, v)| {
                (*k == cbor::Value::Text(key.into())).then(|| match v {
                    cbor::Value::Bytes(b) => b.clone(),
                    cbor::Value::Uint(n) => n.to_be_bytes().to_vec(),
                    _ => panic!("stamp field shape"),
                })
            })
            .expect("stamp field present")
    };
    let as_of_ms = u64::from_be_bytes(stamp("as_of_ms").try_into().expect("u64"));
    let mut inputs = JsonMap::new();
    inputs.insert("bytes".into(), json!(hex(&fx.release.encode())));
    out.push(Vector {
        family: 14,
        name: "projection-release-stamp-complete".into(),
        case_kind: "projection".into(),
        source: "11.7".into(),
        surfaces: vec![
            "core".into(),
            "storage-macos".into(),
            "storage-linux".into(),
            "storage-windows".into(),
        ],
        rng: None,
        inputs,
        expected: Expected::Result(json!({
            "data_frontier": hex(&stamp("data_frontier")),
            "control_frontier": hex(&stamp("control_frontier")),
            "as_of_ms": as_of_ms,
        })),
    });

    // Stamp incompleteness: a PROPERLY SIGNED release whose body
    // lacks as_of_ms — the body-hash stage passes; the CDDL gap
    // fires (`body-invariant`).
    let stripped = {
        let cbor::Value::Map(entries) = &fx.release.body else {
            panic!("release body is a map")
        };
        let kept: Vec<(&str, cbor::Value)> = entries
            .iter()
            .filter_map(|(k, v)| {
                let cbor::Value::Text(name) = k else {
                    panic!("text keys")
                };
                (name != "as_of_ms").then(|| (name.as_str(), v.clone()))
            })
            .collect();
        cbor::map(kept)
    };
    // Re-sign a fresh header over the stripped body (same writer
    // coordinates as the original release — a parallel mint, not a
    // tamper).
    let rig2 = release_fixture("f14-migration-fixture-stripped");
    let mut header2 = rig2.release.header.clone();
    header2.body_hash = [0; 32];
    let sk = {
        // Recreate dev1's signing key deterministically: the rig's
        // seed drawing is name-keyed, so a fresh rig with the same
        // fixture name yields the same key.
        let rig3 = PlaneRig::new("f14-migration-fixture-stripped");
        rig3.dev1.sig_sk.clone()
    };
    let stripped_op = seal_op(header2, stripped, &OpSigner::Ed25519(&sk));
    let mut inputs = JsonMap::new();
    inputs.insert("bytes".into(), json!(hex(&stripped_op.encode())));
    out.push(Vector {
        family: 14,
        name: "projection-stamp-incomplete-rejects".into(),
        case_kind: "projection".into(),
        source: "11.7".into(),
        surfaces: vec![
            "core".into(),
            "storage-macos".into(),
            "storage-linux".into(),
            "storage-windows".into(),
        ],
        rng: None,
        inputs,
        expected: Expected::Negative {
            outcome: "body-invariant".into(),
            disposition: "reject-permanent".into(),
        },
    });

    // Umbrella App C #2 — the offline expiry rule confirmation. NOT
    // yet performed; the fixture pins the working position and the
    // recording obligation (§15, when run).
    let mut inputs = JsonMap::new();
    inputs.insert(
        "procedure".into(),
        json!("umbrella App C #2: offline expiry rule confirmation against real offline usage"),
    );
    inputs.insert(
        "params".into(),
        json!({
            "working_position": "acceptance/witness-deadline expiry + online-only leases for high-impact capabilities + epoch/sequence budgets for offline writers",
        }),
    );
    out.push(Vector {
        family: 14,
        name: "offline-expiry-confirmation-pending".into(),
        case_kind: "offline-confirmation".into(),
        source: "13.3".into(),
        surfaces: vec!["core".into()],
        rng: None,
        inputs,
        expected: Expected::Result(json!({
            "recorded": "PENDING: the §4.5 working position stands unconfirmed; the run's result enters the §15 decision record when performed (Gate-A audit tracks the open item)",
        })),
    });

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_migration_corpus_matches_builders() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vectors");
        for v in corpus_migration() {
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
    fn migration_corpus_checks_clean() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let companion: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("d0a-vector-cases.v1.json")).unwrap(),
        )
        .unwrap();
        for v in corpus_migration() {
            crate::vector::check(&v.to_json(), &companion)
                .unwrap_or_else(|e| panic!("{}: {e}", v.name));
        }
    }
}
