//! Corpus family 7 control-fold walkthroughs, first battery: the
//! hosted-plane ceiling (§7.5) and drill acceptance (§7.1 `c.drill`).
//!
//! - `walkthrough-hosted-solo-boot`: a HOSTED genesis boots
//!   (hosted-browser first certificate, the §7.5 safe verb set on
//!   the genesis grant) and a `propose` claim admits under it;
//!   probes pin the recorded provenance and the un-lifted ceiling.
//! - `hosted-ceiling-grant-verb-excluded`: an enroll compound whose
//!   `grants[]` carries `judge.full` — never grantable under the
//!   ceiling (§7.5 (b)) — rejects `(hosted-ceiling,
//!   reject-permanent)`.
//! - `hosted-ceiling-zone-policy-inadmissible`: `c.zone_policy` is
//!   outside §7.5 (c) ("hosted planes remain on the genesis budgets
//!   posture until re-root", D-43) — the SAME bytes a trusted plane
//!   would admit reject `(hosted-ceiling, reject-permanent)`.
//! - `drill-acceptance`: a trusted plane's `c.drill` — recovery-arm
//!   signature against the CURRENT commitment at the CURRENT repoch
//!   (a proof, not a succession) — admits.
//!
//! Probe values are canonical CBOR (register #17):
//! `plane.provenance` = the genesis descriptor's provenance text;
//! `ceiling.lifted` = whether a recovery succession has been
//! accepted (the §7.5/D-42 lift predicate); `ctrl.head` = the
//! control chain head hash.

use crate::cbor::{self, Value};
use crate::shapes::envelope::Signedop;
use crate::shapes::{DeadlineFallback, Frontierclose, Strictness, TimeWitness, Verb, Zonepolicy};
use crate::tranche::{items, PlaneRig};
use crate::vector::{Expected, Vector};
use serde_json::{json, Map as JsonMap, Value as Json};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn probe(name: &str, value: &Value) -> Json {
    json!({ "name": name, "value": hex(&cbor::encode(value).expect("probe encodes")) })
}

/// A family-7 fold-or-walkthrough vector over two delivery orders.
fn ctrl_vector(
    name: &str,
    case_kind: &str,
    source: &str,
    rig: PlaneRig,
    item_list: &[(&str, &Signedop)],
    per_item: Json,
    probes: Option<Json>,
) -> Vector {
    let mut inputs = JsonMap::new();
    inputs.insert("items".into(), items(item_list));
    let forward: Vec<&str> = item_list.iter().map(|(n, _)| *n).collect();
    let mut reversed = forward.clone();
    reversed.reverse();
    inputs.insert("deliveries".into(), json!([forward, reversed]));
    let mut result = JsonMap::new();
    result.insert("per_item".into(), per_item);
    result.insert("converge".into(), json!(true));
    if let Some(p) = probes {
        result.insert("state_probes".into(), p);
    }
    Vector {
        family: 7,
        name: name.into(),
        case_kind: case_kind.into(),
        source: source.into(),
        surfaces: vec!["core".into()],
        rng: Some(rig.rng.into_json()),
        inputs,
        expected: Expected::Result(Json::Object(result)),
    }
}

/// Hosted genesis boots; a safe-verb claim admits; the ceiling
/// stands un-lifted.
pub fn f7_hosted_solo_boot() -> Vector {
    let name = "f7-hosted-solo-boot";
    let mut rig = PlaneRig::new_hosted(name);
    let d1 = rig.dev1.clone();
    let g1 = rig.genesis_grant.clone();
    let i = rig.claim(&d1, &g1, "i", "the hosted diary opens", 1, None);
    let c1 = rig.genesis_op.clone();
    let head = c1.op_hash();
    ctrl_vector(
        "walkthrough-hosted-solo-boot",
        "walkthrough",
        "7.5",
        rig,
        &[("c1", &c1), ("i", &i)],
        json!([{ "item": "c1" }, { "item": "i" }]),
        Some(json!([
            probe("plane.provenance", &Value::Text("hosted".into())),
            probe("ceiling.lifted", &Value::Bool(false)),
            probe("ctrl.head", &Value::Bytes(head.to_vec())),
        ])),
    )
}

/// An enroll compound minting `judge.full` under the ceiling.
pub fn f7_hosted_grant_verb_excluded() -> Vector {
    let name = "f7-hosted-grant-verb";
    let mut rig = PlaneRig::new_hosted(name);
    let d2 = rig.mint_device("dev2");
    let gj = rig.simple_grant("grantjf", &d2, vec![Verb::Propose, Verb::JudgeFull]);
    let c2 = rig.enroll_new(&d2, vec![gj], "wrap.dev2.eph");
    let c1 = rig.genesis_op.clone();
    ctrl_vector(
        "hosted-ceiling-grant-verb-excluded",
        "fold",
        "7.5",
        rig,
        &[("c1", &c1), ("c2", &c2)],
        json!([
            { "item": "c1" },
            { "item": "c2", "outcome": "hosted-ceiling", "disposition": "reject-permanent" },
        ]),
        None,
    )
}

/// `c.zone_policy` under the ceiling — bytes a trusted plane would
/// admit (witness install with full strict coverage). The enroll is
/// LEGAL here (its grant carries only `propose`, inside the safe
/// set); only the policy install rejects.
pub fn f7_hosted_zone_policy_inadmissible() -> Vector {
    let name = "f7-hosted-zone-policy";
    let mut rig = PlaneRig::new_hosted(name);
    let d1 = rig.dev1.clone();
    let d2 = rig.mint_device("dev2");
    let g2 = rig.simple_grant("grant2", &d2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&d2, vec![g2], "wrap.dev2.eph");
    let policy = Zonepolicy {
        zone_id: rig.zone_id,
        strictness: Strictness::Strict,
        deadline_fallback: DeadlineFallback::Budgets,
        require_cert_deadlines: false,
        grant_epoch_slack: None,
        time_witnesses: Some(vec![TimeWitness::Device(d2.device_id)]),
        connect_service_key: None,
    };
    let closes = [d1.lineage, d2.lineage]
        .iter()
        .map(|l| Frontierclose {
            zone_id: rig.zone_id,
            lineage: *l,
            heads: vec![],
        })
        .collect();
    let c3 = rig.zone_policy_op(policy, closes);
    let c1 = rig.genesis_op.clone();
    ctrl_vector(
        "hosted-ceiling-zone-policy-inadmissible",
        "fold",
        "7.5",
        rig,
        &[("c1", &c1), ("c2", &c2), ("c3", &c3)],
        json!([
            { "item": "c1" },
            { "item": "c2" },
            { "item": "c3", "outcome": "hosted-ceiling", "disposition": "reject-permanent" },
        ]),
        None,
    )
}

/// `c.drill` on a trusted plane: recovery-arm proof at the current
/// repoch, chain advances.
pub fn f7_drill_acceptance() -> Vector {
    let name = "f7-drill";
    let mut rig = PlaneRig::new(name);
    let c2 = rig.drill_op(0);
    let c1 = rig.genesis_op.clone();
    let head = c2.op_hash();
    ctrl_vector(
        "walkthrough-drill-acceptance",
        "walkthrough",
        "7.1",
        rig,
        &[("c1", &c1), ("c2", &c2)],
        json!([{ "item": "c1" }, { "item": "c2" }]),
        Some(json!([
            probe("ceiling.lifted", &Value::Bool(false)),
            probe("ctrl.head", &Value::Bytes(head.to_vec())),
        ])),
    )
}

pub fn corpus_ctrl() -> Vec<Vector> {
    vec![
        f7_hosted_solo_boot(),
        f7_hosted_grant_verb_excluded(),
        f7_hosted_zone_policy_inadmissible(),
        f7_drill_acceptance(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_ctrl_corpus_matches_builders() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vectors");
        for v in corpus_ctrl() {
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
    fn ctrl_corpus_checks_clean() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let companion: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("d0a-vector-cases.v1.json")).unwrap(),
        )
        .unwrap();
        for v in corpus_ctrl() {
            crate::vector::check(&v.to_json(), &companion)
                .unwrap_or_else(|e| panic!("{}: {e}", v.name));
        }
    }
}
