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
use crate::domains::{h_tag, Tag};
use crate::shapes::control::{AdminKey, Crecovsucc};
use crate::shapes::envelope::{ActorKind, Signedop};
use crate::shapes::memory::Merasereq;
use crate::shapes::{
    DeadlineFallback, Erasemref, Frontierclose, Sigalg, Strictness, TimeWitness, ToValue, Verb,
    Zonepolicy,
};
use crate::suite;
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

/// C2: two DIFFERENT control ops at one position freeze BOTH
/// branches (§7.4 — "no further control ops on either branch");
/// tenant writes citing UNCONTESTED authority continue. Both
/// delivery orders converge on the freeze-both state — whichever op
/// admitted first is re-classified when the fork is discovered.
pub fn f7_c2_freeze_both() -> Vector {
    let name = "f7-c2-freeze";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let g1 = rig.genesis_grant.clone();
    // x2: a legal epoch bump occupying position 2 WITHOUT advancing
    // the rig chain (coverage = dev1's lineage, the only live one at
    // that point).
    let x2 = {
        let fc = Frontierclose {
            zone_id: rig.zone_id,
            lineage: d1.lineage,
            heads: vec![],
        };
        rig.epoch_bump_candidate("x2", 2, vec![fc])
    };
    // e2: the enrollment holding the SAME position on the rig chain.
    let d2 = rig.mint_device("dev2");
    let g2 = rig.simple_grant("grant2", &d2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&d2, vec![g2], "wrap.dev2.eph");
    // i: a claim under the UNCONTESTED genesis authority — tenant
    // writes continue under the last uncontested frontier.
    let i = rig.claim(
        &d1,
        &g1,
        "i",
        "the harbor light holds through the fork",
        1,
        None,
    );
    let c1 = rig.genesis_op.clone();
    let head = c1.op_hash();
    ctrl_vector(
        "walkthrough-c2-freeze-both",
        "walkthrough",
        "7.4",
        rig,
        &[("c1", &c1), ("e2", &c2), ("x2", &x2), ("i", &i)],
        json!([
            { "item": "c1" },
            { "item": "e2", "outcome": "ctrl-fork", "disposition": "freeze-control" },
            { "item": "i" },
            { "item": "x2", "outcome": "ctrl-fork", "disposition": "freeze-control" },
        ]),
        Some(json!([
            probe("ctrl.frozen", &Value::Bool(true)),
            probe("ctrl.head", &Value::Bytes(head.to_vec())),
        ])),
    )
}

/// C3′ below the head: the recovery bases at the genesis, cutting
/// the enrollment above it — the cut control op re-classifies
/// `(cutoff, quarantine-reproposal)` (the D-140 recover-boundary
/// reading; the spec names no pair for a cut CONTROL op — register/
/// audit item), and the cut branch's tenant write re-pends on its
/// dissolved citations (D-138/D-199). Empty `tenant_cutoffs` = the
/// pure revivable omission blanket. The reversed order exercises the
/// §7.4 precedence exception: the enrollment arriving AFTER the
/// accepted recovery classifies as cut-branch material at the
/// recovery's own position, never C2.
pub fn f7_c3_branch_cut_below_head() -> Vector {
    let name = "f7-c3-branch-cut";
    let mut rig = PlaneRig::new(name);
    let d2 = rig.mint_device("dev2");
    let g2 = rig.simple_grant("grant2", &d2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&d2, vec![g2.clone()], "wrap.dev2.eph");
    let i2 = rig.claim(&d2, &g2, "i2", "the cut branch wrote this", 1, None);
    let c1 = rig.genesis_op.clone();
    let admin2_seed = rig.rng.draw32("admin2.sig_seed");
    let (_a2_sk, a2_pk) = suite::ed25519::keypair(&admin2_seed);
    let recovery2_seed = rig.rng.draw32("recovery2.sig_seed");
    let (_r2_sk, r2_pk) = suite::ed25519::keypair(&recovery2_seed);
    let r = rig.recovery_op_tagged(
        "r",
        Crecovsucc {
            base_seq: 1,
            base_op: c1.op_hash(),
            epoch: 2,
            repoch: 1,
            new_admin: AdminKey {
                alg: Sigalg::Ed25519,
                pk: a2_pk.to_vec(),
            },
            new_recovery_commitment: h_tag(Tag::Drill, &r2_pk),
            tenant_cutoffs: vec![],
            adopted_renewals: None,
            retired_keys: None,
            adopted_rotations: vec![],
        },
    );
    let r_hash = r.op_hash();
    ctrl_vector(
        "walkthrough-c3-branch-cut-below-head",
        "walkthrough",
        "7.4",
        rig,
        &[("c1", &c1), ("e2", &c2), ("i2", &i2), ("r", &r)],
        json!([
            { "item": "c1" },
            { "item": "e2", "outcome": "cutoff", "disposition": "quarantine-reproposal" },
            { "item": "i2", "outcome": "ref-unresolved", "disposition": "pending-dependency" },
            { "item": "r" },
        ]),
        Some(json!([
            probe("repoch", &Value::Uint(1)),
            probe("ctrl.frozen", &Value::Bool(false)),
            probe("ctrl.head", &Value::Bytes(r_hash.to_vec())),
        ])),
    )
}

/// The C2 freeze rig plus one post-freeze operation `g4`, optionally
/// signature-tampered.
fn post_freeze_fixture(
    name: &'static str,
    tamper: bool,
) -> (PlaneRig, Vec<(&'static str, Signedop)>) {
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let x2 = {
        let fc = Frontierclose {
            zone_id: rig.zone_id,
            lineage: d1.lineage,
            heads: vec![],
        };
        rig.epoch_bump_candidate("x2", 2, vec![fc])
    };
    let d2 = rig.mint_device("dev2");
    let g2 = rig.simple_grant("grant2", &d2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&d2, vec![g2], "wrap.dev2.eph");
    let g4g = rig.simple_grant("grant4", &d1, vec![Verb::Assert]);
    let mut g4 = rig.grant_op(g4g);
    if tamper {
        g4.signature[0] ^= 1;
    }
    let c1 = rig.genesis_op.clone();
    (rig, vec![("c1", c1), ("e2", c2), ("x2", x2), ("g4", g4)])
}

/// D4 (a): an otherwise-VALID control operation arriving on the
/// frozen plane classifies (ctrl-fork, freeze-control) — only
/// recovery resolves C2.
pub fn f7_post_freeze_valid_op_frozen() -> Vector {
    let (rig, ops) = post_freeze_fixture("f7-post-freeze-valid", false);
    let refs: Vec<(&str, &Signedop)> = ops.iter().map(|(n, o)| (*n, o)).collect();
    ctrl_vector(
        "c2-post-freeze-valid-op-frozen",
        "fold",
        "7.4",
        rig,
        &refs,
        json!([
            { "item": "c1" },
            { "item": "e2", "outcome": "ctrl-fork", "disposition": "freeze-control" },
            { "item": "g4", "outcome": "ctrl-fork", "disposition": "freeze-control" },
            { "item": "x2", "outcome": "ctrl-fork", "disposition": "freeze-control" },
        ]),
        None,
    )
}

/// D4 (b): a SIGNATURE-INVALID post-freeze operation keeps its
/// signature outcome — a forgery is never fork evidence and the
/// signature stage precedes the frozen-plane classification (D-76).
pub fn f7_post_freeze_sig_invalid_kept() -> Vector {
    let (rig, ops) = post_freeze_fixture("f7-post-freeze-sig", true);
    let refs: Vec<(&str, &Signedop)> = ops.iter().map(|(n, o)| (*n, o)).collect();
    ctrl_vector(
        "c2-post-freeze-sig-invalid-kept",
        "fold",
        "7.4",
        rig,
        &refs,
        json!([
            { "item": "c1" },
            { "item": "e2", "outcome": "ctrl-fork", "disposition": "freeze-control" },
            { "item": "g4", "outcome": "sig-invalid", "disposition": "reject-permanent" },
            { "item": "x2", "outcome": "ctrl-fork", "disposition": "freeze-control" },
        ]),
        None,
    )
}

/// The §5.4 manifest-admission face (D-203 ratified the P1 profile;
/// this is its first implement-before-Gate-A mechanism): a rotation
/// whose typed `erase_manifest` cites an ACCEPTED `m.erase_request`
/// with `target_op` in its `targets` ADMITS. The reversed order
/// delivers the rotation before the request — it pends
/// `ref-unresolved` and admits at the fixpoint (the citation is
/// verifiable-when-held). The `item_addr` is author-attested (§5.6
/// index territory), opaque to admission.
pub fn f7_kek_rotate_manifest_admits() -> Vector {
    let name = "f7-kek-rotate-manifest";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let g1 = rig.genesis_grant.clone();
    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let i = rig.claim(
        &dev2,
        &grant2,
        "i",
        "the ledger holds a stray entry",
        1,
        None,
    );
    let e = rig.tenant_op_as(
        ActorKind::Human,
        &d1,
        &g1,
        "e",
        Merasereq::OP_TYPE,
        Merasereq {
            targets: vec![i.op_hash()],
        }
        .to_value(),
        1,
        None,
    );
    let k = {
        let kek_e2 = rig.rng.draw32("kek.zone.e2");
        let (id, pk) = (d1.device_id, d1.kem_pk);
        let w = rig.wrap_at(id, &pk, 2, &kek_e2, "wrap.dev1.e2.eph");
        let entry = Erasemref {
            item_addr: rig.rng.draw32("erase.item_addr"),
            erase_op: e.op_hash(),
            target_op: i.op_hash(),
        };
        rig.kek_rotate_erasing(2, vec![w], vec![entry])
    };
    let c1 = rig.genesis_op.clone();
    ctrl_vector(
        "kek-rotate-manifest-admits",
        "fold",
        "5.4",
        rig,
        &[("c1", &c1), ("c2", &c2), ("i", &i), ("e", &e), ("k", &k)],
        json!([
            { "item": "c1" },
            { "item": "c2" },
            { "item": "i" },
            { "item": "e" },
            { "item": "k" },
        ]),
        None,
    )
}

/// The §5.4 membership negative: the manifest entry's `target_op` is
/// a held, in-scope claim that the cited erase request does NOT
/// list — `(body-invariant, reject-permanent)` on the rotation
/// (portably checkable at admission, D-66).
pub fn f7_kek_rotate_manifest_target_outside() -> Vector {
    let name = "f7-kek-rotate-manifest-outside";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let g1 = rig.genesis_grant.clone();
    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let i = rig.claim(&dev2, &grant2, "i", "the requested target", 1, None);
    let i2 = rig.claim(
        &dev2,
        &grant2,
        "i2",
        "the unrequested neighbor",
        2,
        Some(i.op_hash()),
    );
    let e = rig.tenant_op_as(
        ActorKind::Human,
        &d1,
        &g1,
        "e",
        Merasereq::OP_TYPE,
        Merasereq {
            targets: vec![i.op_hash()],
        }
        .to_value(),
        1,
        None,
    );
    let k = {
        let kek_e2 = rig.rng.draw32("kek.zone.e2");
        let (id, pk) = (d1.device_id, d1.kem_pk);
        let w = rig.wrap_at(id, &pk, 2, &kek_e2, "wrap.dev1.e2.eph");
        let entry = Erasemref {
            item_addr: rig.rng.draw32("erase.item_addr"),
            erase_op: e.op_hash(),
            target_op: i2.op_hash(),
        };
        rig.kek_rotate_erasing(2, vec![w], vec![entry])
    };
    let c1 = rig.genesis_op.clone();
    ctrl_vector(
        "kek-rotate-manifest-target-outside-rejects",
        "fold",
        "5.4",
        rig,
        &[
            ("c1", &c1),
            ("c2", &c2),
            ("i", &i),
            ("i2", &i2),
            ("e", &e),
            ("k", &k),
        ],
        json!([
            { "item": "c1" },
            { "item": "c2" },
            { "item": "i" },
            { "item": "i2" },
            { "item": "e" },
            { "item": "k", "outcome": "body-invariant", "disposition": "reject-permanent" },
        ]),
        None,
    )
}

pub fn corpus_ctrl() -> Vec<Vector> {
    vec![
        f7_hosted_solo_boot(),
        f7_hosted_grant_verb_excluded(),
        f7_hosted_zone_policy_inadmissible(),
        f7_drill_acceptance(),
        f7_c2_freeze_both(),
        f7_c3_branch_cut_below_head(),
        f7_post_freeze_valid_op_frozen(),
        f7_post_freeze_sig_invalid_kept(),
        f7_kek_rotate_manifest_admits(),
        f7_kek_rotate_manifest_target_outside(),
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
