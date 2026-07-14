//! Corpus families 8 and 11's Merkle/export lanes: §2.4 recovery
//! derivation (BIP-39 exact, passphrase ""), the recovery commitment,
//! the §11.8 per-record proof KATs, and the construct-and-rederive
//! export/import ceremony (D-127: sources + keys → release → the
//! harness independently re-derives digest, paths, and release_op).
//!
//! Fixture conventions (register entries):
//! - family-8 `keys` names: `seed` (64 B), `ed25519_seed` (32 B),
//!   `recovery_pk` (32 B); `phrase` is the NFKD-normalized mnemonic
//!   (ASCII here, so normalization is the identity).
//! - §2.4's checksum-invalid rejection has NO expressible companion
//!   case (phrase-derive requires `result.keys`) — a Gate-A audit
//!   item, not a fixture.
//! - merkle-proof vectors carry no record_count (the §11.8 admission
//!   lane reads it from the signed release); the KAT lane quantifies
//!   over widths 1..=128 — `valid` = some width reproduces the root
//!   under exact consumption.

use crate::domains::{h_tag, Tag};
use crate::keyschedule;
use crate::shapes::envelope::ActorKind;
use crate::shapes::identity::{Endpoint, Flow};
use crate::shapes::memory::{merkle_proof, merkle_root, Bundleleaf, Bundlerec, Mimport};
use crate::shapes::{Class, Kind, ToValue, Verb};
use crate::suite;
use crate::tranche::{admits, draw_id, items, PlaneRig, T0_MS};
use crate::vector::{Expected, Vector};
use serde_json::{json, Map as JsonMap};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// --------------------------------------------------------- family 8

fn phrase_vector(name: &str, entropy: &[u8; 32]) -> Vector {
    let mnemonic = bip39::Mnemonic::from_entropy(entropy).expect("32-byte entropy");
    let phrase = mnemonic.to_string();
    let seed = mnemonic.to_seed("");
    let ed_seed = keyschedule::recovery_ed25519_seed(&seed);
    let (_sk, pk) = suite::ed25519::keypair(&ed_seed);

    let mut inputs = JsonMap::new();
    inputs.insert("phrase".into(), json!(phrase));
    inputs.insert("entropy".into(), json!(hex(entropy)));
    Vector {
        family: 8,
        name: name.into(),
        case_kind: "phrase-derive".into(),
        source: "2.4".into(),
        surfaces: vec!["native-crypto".into(), "browser".into()],
        rng: None,
        inputs,
        expected: Expected::Result(json!({
            "keys": {
                "seed": hex(&seed),
                "ed25519_seed": hex(&ed_seed),
                "recovery_pk": hex(&pk),
            }
        })),
    }
}

fn commitment_vector() -> Vector {
    let entropy = [0x42u8; 32];
    let mnemonic = bip39::Mnemonic::from_entropy(&entropy).expect("32-byte entropy");
    let seed = mnemonic.to_seed("");
    let ed_seed = keyschedule::recovery_ed25519_seed(&seed);
    let (_sk, pk) = suite::ed25519::keypair(&ed_seed);
    let commitment = h_tag(Tag::Drill, &pk);

    let mut inputs = JsonMap::new();
    inputs.insert("recovery_pk".into(), json!(hex(&pk)));
    Vector {
        family: 8,
        name: "commitment-from-recovery-pk".into(),
        case_kind: "commitment-derive".into(),
        source: "2.4".into(),
        surfaces: vec!["native-crypto".into(), "browser".into()],
        rng: None,
        inputs,
        expected: Expected::Bytes(commitment.to_vec()),
    }
}

// ------------------------------------------- family 11 merkle-proof

/// A fixed 3-record bundle (widths 3 → 2 → 1: leaf 2 promotes at
/// level 0 — the odd-node rule is inside the proof shape).
fn three_leaf_bundle() -> ([u8; 16], Vec<Bundleleaf>, Vec<[u8; 32]>, [u8; 32]) {
    let export_id = [0x0e; 16];
    let leaves_obj: Vec<Bundleleaf> = (0..3u64)
        .map(|i| {
            let mut op = [0u8; 32];
            op[0] = 0x10 + i as u8;
            Bundleleaf {
                export_id,
                rec_index: i,
                rec: Bundlerec {
                    op,
                    kind: Kind::Observation,
                    statement: format!("record number {i} of the fixed bundle"),
                    class_floor: Class::Private,
                },
            }
        })
        .collect();
    let leaves: Vec<[u8; 32]> = leaves_obj.iter().map(|l| l.leaf_hash()).collect();
    let root = merkle_root(&leaves);
    (export_id, leaves_obj, leaves, root)
}

fn merkle_vectors() -> Vec<Vector> {
    let (_eid, leaves_obj, leaves, root) = three_leaf_bundle();
    let leaf2_bytes = crate::cbor::encode(&leaves_obj[2].to_value()).expect("leaf encodes");
    let proof2 = merkle_proof(&leaves, 2);
    assert_eq!(proof2.len(), 1, "leaf 2 promotes at level 0");

    let mk = |name: &str, rec_index: u64, proof: &[[u8; 32]], expected: Expected| {
        let mut inputs = JsonMap::new();
        inputs.insert("bundleleaf".into(), json!(hex(&leaf2_bytes)));
        inputs.insert("rec_index".into(), json!(rec_index));
        inputs.insert(
            "proof".into(),
            json!(proof.iter().map(|p| hex(p)).collect::<Vec<_>>()),
        );
        inputs.insert("root".into(), json!(hex(&root)));
        Vector {
            family: 11,
            name: name.into(),
            case_kind: "merkle-proof".into(),
            source: "11.8".into(),
            surfaces: vec!["core".into(), "browser".into()],
            rng: None,
            inputs,
            expected,
        }
    };

    vec![
        mk(
            "merkle-promotion-path-valid",
            2,
            &proof2,
            Expected::Result(json!({ "valid": true })),
        ),
        mk(
            "merkle-wrong-index-invalid",
            1,
            &proof2,
            Expected::Result(json!({ "valid": false })),
        ),
        mk(
            "merkle-leftover-sibling-rejects",
            2,
            &[proof2[0], [0xab; 32]],
            Expected::Negative {
                outcome: "body-invariant".into(),
                disposition: "reject-permanent".into(),
            },
        ),
    ]
}

// ----------------------------------------- family 11 export-import

/// The construct-and-rederive ceremony (D-127/D-156): two claims,
/// one release, two imports with real promotion-free paths. The
/// harness re-derives `content_digest` from the HELD sources and
/// `release_op` from the release bytes — independently of the values
/// carried here.
pub fn f11_export_import_rederive() -> Vector {
    let name = "export-import-construct-and-rederive";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let g1 = rig.genesis_grant.clone();
    let (gz, home) = (rig.zone_id, rig.home_space);

    // Destination zone + space + dev2's import grant (one per zone).
    let z2 = draw_id(&mut rig.rng, "zone2.zone_id");
    let kek2 = rig.rng.draw32("kek.zone2.e1");
    let w1 = rig.wrap_in(z2, &kek2, d1.device_id, &d1.kem_pk, 1, "wrap.z2.dev1.eph");
    let cz = rig.zone_create(z2, vec![w1]);
    let z2_space = draw_id(&mut rig.rng, "zone2.space_id");
    let z2_name_hash = rig.rng.draw32("zone2.space.name_hash");
    let cs = rig.space_create(crate::shapes::Spacedef {
        space_id: z2_space,
        zone_id: z2,
        name_hash: z2_name_hash,
        space_class: crate::shapes::Spaceclass::Project,
        class_minimum: Class::Private,
        status_policy: crate::shapes::Polref {
            id: "workflow-v1".into(),
            version: 1,
            hash: crate::scenario::workflow_v1().hash(),
        },
    });
    let dev2 = rig.mint_device("dev2");
    let g2 = rig.grant_in("grant2", &dev2, vec![Verb::Import], z2, vec![z2_space]);
    let w2 = rig.wrap_in(
        z2,
        &kek2,
        dev2.device_id,
        &dev2.kem_pk,
        1,
        "wrap.z2.dev2.eph",
    );
    let c2 = rig.enroll_new_with_wraps(&dev2, vec![g2.clone()], vec![w2]);

    // dev1's flow-carrying export grant.
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
            zone_id: z2,
            space_id: z2_space,
        },
        kinds: None,
        class_ceiling: Class::Sensitive,
        expiry_deadline_ms: T0_MS + 10 * 86_400_000,
    }]);
    let gf = rig.grant_op(gf_grant.clone());

    // Two source claims; the bundle ranks by op hash (D-156).
    const S1: &str = "coastal survey marker seventeen verified in place";
    const S2: &str = "tidal gauge calibration completed at spring high";
    let i1 = rig.claim(&d1, &g1, "i1", S1, 1, None);
    let i2 = rig.claim(&d1, &g1, "i2", S2, 2, Some(i1.op_hash()));
    let export_id = draw_id(&mut rig.rng, "rel.export_id");
    let mut ranked = [(i1.op_hash(), S1), (i2.op_hash(), S2)];
    ranked.sort_by_key(|(op, _)| *op);
    let leaves: Vec<[u8; 32]> = ranked
        .iter()
        .enumerate()
        .map(|(rank, (op, stmt))| {
            Bundleleaf {
                export_id,
                rec_index: rank as u64,
                rec: Bundlerec {
                    op: *op,
                    kind: Kind::Observation,
                    statement: (*stmt).into(),
                    class_floor: Class::Private,
                },
            }
            .leaf_hash()
        })
        .collect();
    let digest = merkle_root(&leaves);
    let data_frontier = rig.rng.draw32("rel.data_frontier");
    let rel = rig.release_op_signed(
        &d1,
        &gf_grant,
        "rel",
        export_id,
        vec![i1.op_hash(), i2.op_hash()],
        digest,
        z2,
        z2_space,
        data_frontier,
        3,
        Some(i2.op_hash()),
    );

    // The two imports, each with its exact sibling path.
    let mk_import = |rig: &mut PlaneRig, tag: &str, rank: usize, seq: u64, prev| {
        let (op, stmt) = ranked[rank];
        let body = Mimport {
            source_op: op,
            class_floor: Class::Private,
            kind: Kind::Observation,
            statement: stmt.into(),
            sensitivity: Class::Private,
            rec_index: rank as u64,
            proof: merkle_proof(&leaves, rank),
            from_plane: rig.plane_id,
            export_id,
            release_op: rel.op_hash(),
            digest,
        };
        rig.tenant_op_in(
            z2,
            z2_space,
            ActorKind::Daemon,
            &dev2,
            &g2,
            tag,
            Mimport::OP_TYPE,
            body.to_value(),
            seq,
            prev,
        )
    };
    let m1 = mk_import(&mut rig, "m1", 0, 1, None);
    let m2 = mk_import(&mut rig, "m2", 1, 2, Some(m1.op_hash()));

    let c1 = rig.genesis_op.clone();
    let mut inputs = JsonMap::new();
    inputs.insert(
        "items".into(),
        items(&[
            ("c1", &c1),
            ("cz", &cz),
            ("cs", &cs),
            ("c2", &c2),
            ("gf", &gf),
            ("i1", &i1),
            ("i2", &i2),
            ("rel", &rel),
            ("m1", &m1),
            ("m2", &m2),
        ]),
    );
    inputs.insert(
        "deliveries".into(),
        json!([["c1", "cz", "cs", "c2", "gf", "i1", "i2", "rel", "m1", "m2"]]),
    );
    Vector {
        family: 11,
        name: name.into(),
        case_kind: "export-import".into(),
        source: "11.8".into(),
        surfaces: vec!["core".into()],
        rng: Some(rig.rng.into_json()),
        inputs,
        expected: Expected::Result(json!({
            "per_item": [
                admits("c1"),
                admits("cz"),
                admits("cs"),
                admits("c2"),
                admits("gf"),
                admits("i1"),
                admits("i2"),
                admits("rel"),
                admits("m1"),
                admits("m2"),
            ],
            "converge": true,
            "content_digest": hex(&digest),
            "release_op": hex(&rel.op_hash()),
        })),
    }
}

/// Families 8 + 11 (Merkle/export lanes), family-ordered.
pub fn corpus_recovery() -> Vec<Vector> {
    let mut out = vec![
        phrase_vector("phrase-derive-zero-entropy", &[0u8; 32]),
        phrase_vector("phrase-derive-fixed-entropy", &[0x42u8; 32]),
        commitment_vector(),
    ];
    out.extend(merkle_vectors());
    out.push(f11_export_import_rederive());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_recovery_corpus_matches_builders() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vectors");
        for v in corpus_recovery() {
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
    fn recovery_corpus_checks_clean() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let companion: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("d0a-vector-cases.v1.json")).unwrap(),
        )
        .unwrap();
        for v in corpus_recovery() {
            crate::vector::check(&v.to_json(), &companion)
                .unwrap_or_else(|e| panic!("{}: {e}", v.name));
        }
    }

    /// The BIP-39 stage against the reference test vector: all-zero
    /// 256-bit entropy → the canonical 24-word mnemonic.
    #[test]
    fn bip39_zero_entropy_reference() {
        let m = bip39::Mnemonic::from_entropy(&[0u8; 32]).unwrap();
        let words: Vec<&str> = m.words().collect();
        assert_eq!(words.len(), 24);
        assert!(words[..23].iter().all(|w| *w == "abandon"));
        assert_eq!(words[23], "art");
    }
}
