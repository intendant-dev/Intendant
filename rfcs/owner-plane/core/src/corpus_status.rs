//! Corpus family 11 status-derive (§11.2): the five-step status fold
//! over admitted, policy-counted judgments — accepted, the
//! anti-suppression pair (one dispute recorded-not-counting under
//! owner-v1, the same class counting under workflow-v1), the
//! author-retract, supersession, revival on replacement loss, and
//! the polref-mismatch pend.
//!
//! Fixture conventions (register entries):
//! - every vector's `as_of_ms` = T0 + 1 day (no claim carries
//!   temporal fields yet — the parameter is threaded, inert).
//! - `derived` rows name claims only.
//! - dev1 judges as OWNER (human actor on the genesis grant's
//!   judge.full); dev2's human judgments are SAFE-HUMAN (grant2
//!   carries no full rights); dev2's daemon-kind ops are the bare
//!   autonomous writer (§11.4 has no explicit row — the engines
//!   derive `session`; audit item).

use crate::shapes::envelope::ActorKind;
use crate::shapes::memory::{BasicVerdict, Mjudge};
use crate::shapes::{Polref, ToValue, Verb};
use crate::tranche::{admits, items, PlaneRig, T0_MS};
use crate::vector::{Expected, Vector};
use serde_json::{json, Map as JsonMap, Value as Json};

fn wf_polref() -> Polref {
    let p = crate::scenario::workflow_v1();
    Polref {
        id: "workflow-v1".into(),
        version: 1,
        hash: p.hash(),
    }
}

fn ov_polref() -> Polref {
    let p = crate::scenario::owner_v1();
    Polref {
        id: "owner-v1".into(),
        version: 1,
        hash: p.hash(),
    }
}

fn judge_basic(verdict: BasicVerdict, target: [u8; 32], policy: Polref) -> crate::cbor::Value {
    Mjudge::Basic {
        verdict,
        target,
        policy,
        reason: None,
        evidence: None,
    }
    .to_value()
}

const AS_OF: u64 = T0_MS + 86_400_000;

#[allow(clippy::too_many_arguments)]
fn status_vector(
    name: &str,
    rig: PlaneRig,
    item_list: &[(&str, &crate::shapes::envelope::Signedop)],
    delivery: Json,
    derived: Json,
    per_item: Json,
) -> Vector {
    let mut inputs = JsonMap::new();
    inputs.insert("items".into(), items(item_list));
    let reversed: Vec<Json> = delivery
        .as_array()
        .expect("delivery is an array")
        .iter()
        .rev()
        .cloned()
        .collect();
    inputs.insert("deliveries".into(), json!([delivery, reversed]));
    inputs.insert("as_of_ms".into(), json!(AS_OF));
    let mut result = JsonMap::new();
    result.insert("derived".into(), derived);
    result.insert("converge".into(), json!(true));
    // per_item rides only where a judgment is expected NOT to admit
    // — the status-derive contract doesn't carry it; encode those as
    // separate fold vectors instead. (Kept as a parameter for the
    // polref-mismatch vector, which needs the pend visible.)
    let _ = per_item;
    Vector {
        family: 11,
        name: name.into(),
        case_kind: "status-derive".into(),
        source: "11.2".into(),
        surfaces: vec!["core".into()],
        rng: Some(rig.rng.into_json()),
        inputs,
        expected: Expected::Result(Json::Object(result)),
    }
}

/// Owner accept on the home space (workflow-v1): accepted.
pub fn f11_status_owner_accept() -> Vector {
    let name = "status-owner-accept";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let g1 = rig.genesis_grant.clone();
    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let i = rig.claim(&dev2, &grant2, "i", "the survey datum stands", 1, None);
    let j = rig.tenant_op_as(
        ActorKind::Human,
        &d1,
        &g1,
        "j",
        Mjudge::OP_TYPE,
        judge_basic(BasicVerdict::Accept, i.op_hash(), wf_polref()),
        1,
        None,
    );
    let c1 = rig.genesis_op.clone();
    status_vector(
        name,
        rig,
        &[("c1", &c1), ("c2", &c2), ("i", &i), ("j", &j)],
        json!(["c1", "c2", "i", "j"]),
        json!([{ "item": "i", "value": "accepted" }]),
        Json::Null,
    )
}

/// The anti-suppression pair, counting side: a SAFE-HUMAN dispute on
/// the home space COUNTS under workflow-v1 (dispute classes = owner,
/// safe-human) — disputed, despite the later owner accept (rule 3
/// precedes rule 4; no causal ancestry exempts the accept).
pub fn f11_status_safe_human_dispute_counts() -> Vector {
    let name = "status-safe-human-dispute-counts";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let g1 = rig.genesis_grant.clone();
    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose, Verb::JudgeSafe]);
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let i = rig.claim(
        &dev2,
        &grant2,
        "i",
        "the gauge reading is contested",
        1,
        None,
    );
    // dev2's HUMAN dispute (safe-human class; judge.safe row: human
    // evidence + observation target).
    let jd = rig.tenant_op_as(
        ActorKind::Human,
        &dev2,
        &grant2,
        "jd",
        Mjudge::OP_TYPE,
        judge_basic(BasicVerdict::Dispute, i.op_hash(), wf_polref()),
        2,
        Some(i.op_hash()),
    );
    let ja = rig.tenant_op_as(
        ActorKind::Human,
        &d1,
        &g1,
        "ja",
        Mjudge::OP_TYPE,
        judge_basic(BasicVerdict::Accept, i.op_hash(), wf_polref()),
        1,
        None,
    );
    let c1 = rig.genesis_op.clone();
    status_vector(
        name,
        rig,
        &[
            ("c1", &c1),
            ("c2", &c2),
            ("i", &i),
            ("jd", &jd),
            ("ja", &ja),
        ],
        json!(["c1", "c2", "i", "jd", "ja"]),
        json!([{ "item": "i", "value": "disputed" }]),
        Json::Null,
    )
}

/// The anti-suppression pair, recorded side: the same safe-human
/// dispute in an OWNER-v1 space is admitted and recorded but counts
/// nothing (dispute classes = owner only) — the owner accept lands
/// the status at accepted.
pub fn f11_status_dispute_recorded_not_counting() -> Vector {
    let name = "status-dispute-recorded-not-counting";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let (gz, _home) = (rig.zone_id, rig.home_space);

    // A second personal space bound to owner-v1.
    let s2 = crate::tranche::draw_id(&mut rig.rng, "space2.space_id");
    let s2_name = rig.rng.draw32("space2.name_hash");
    let cs = rig.space_create(crate::shapes::Spacedef {
        space_id: s2,
        zone_id: gz,
        name_hash: s2_name,
        space_class: crate::shapes::Spaceclass::Personal,
        class_minimum: crate::shapes::Class::Private,
        status_policy: ov_polref(),
    });

    // dev2 writes + safe-judges there; dev1 owner-judges there.
    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.grant_in(
        "grant2",
        &dev2,
        vec![Verb::Propose, Verb::JudgeSafe],
        gz,
        vec![s2],
    );
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let gj_grant = rig.grant_in("grantjudge", &d1, vec![Verb::JudgeFull], gz, vec![s2]);
    let gj = rig.grant_op(gj_grant.clone());
    let body = crate::shapes::memory::Mclaim {
        kind: crate::shapes::Kind::Observation,
        statement: "the annex inventory is complete".into(),
        sensitivity: crate::shapes::Class::Private,
        observed_at_ms: None,
        valid_from_ms: None,
        valid_until_ms: None,
        expires_at_ms: None,
        session: None,
        project: None,
        model: None,
        evidence: vec![],
        supersedes: None,
        labels: None,
    };
    let i = rig.tenant_op_in(
        gz,
        s2,
        ActorKind::Daemon,
        &dev2,
        &grant2,
        "i",
        crate::shapes::memory::Mclaim::OP_TYPE,
        body.to_value(),
        1,
        None,
    );
    let jd = rig.tenant_op_in(
        gz,
        s2,
        ActorKind::Human,
        &dev2,
        &grant2,
        "jd",
        Mjudge::OP_TYPE,
        judge_basic(BasicVerdict::Dispute, i.op_hash(), ov_polref()),
        2,
        Some(i.op_hash()),
    );
    let ja = rig.tenant_op_in(
        gz,
        s2,
        ActorKind::Human,
        &d1,
        &gj_grant,
        "ja",
        Mjudge::OP_TYPE,
        judge_basic(BasicVerdict::Accept, i.op_hash(), ov_polref()),
        1,
        None,
    );
    let c1 = rig.genesis_op.clone();
    status_vector(
        name,
        rig,
        &[
            ("c1", &c1),
            ("cs", &cs),
            ("c2", &c2),
            ("gj", &gj),
            ("i", &i),
            ("jd", &jd),
            ("ja", &ja),
        ],
        json!(["c1", "cs", "c2", "gj", "i", "jd", "ja"]),
        json!([{ "item": "i", "value": "accepted" }]),
        Json::Null,
    )
}

/// The author retract through the second AUTHOR arm: the claim was
/// authored daemon-kind; the retract arrives HUMAN-kind on the same
/// lineage (P inequality — same lineage + human evidence is what
/// makes it the author). workflow-v1 counts safe-human author
/// retracts → retired.
pub fn f11_status_author_retract() -> Vector {
    let name = "status-author-retract";
    let mut rig = PlaneRig::new(name);
    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let i = rig.claim(
        &dev2,
        &grant2,
        "i",
        "the draft figure was provisional",
        1,
        None,
    );
    let jr = rig.tenant_op_as(
        ActorKind::Human,
        &dev2,
        &grant2,
        "jr",
        Mjudge::OP_TYPE,
        judge_basic(BasicVerdict::Retract, i.op_hash(), wf_polref()),
        2,
        Some(i.op_hash()),
    );
    let c1 = rig.genesis_op.clone();
    status_vector(
        name,
        rig,
        &[("c1", &c1), ("c2", &c2), ("i", &i), ("jr", &jr)],
        json!(["c1", "c2", "i", "jr"]),
        json!([{ "item": "i", "value": "retired" }]),
        Json::Null,
    )
}

/// Supersession and its loss: owner supersedes i→r and accepts r
/// (i superseded); the second vector's extra owner retract of r
/// drops status(r) from accepted — the supersession no longer holds
/// and i REVIVES to candidate (D-21, automatic and surfaced).
fn supersede_rig(name: &str, with_revival: bool) -> Vector {
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let g1 = rig.genesis_grant.clone();
    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let i = rig.claim(&dev2, &grant2, "i", "the first reading", 1, None);
    let r = rig.claim(
        &dev2,
        &grant2,
        "r",
        "the corrected reading",
        2,
        Some(i.op_hash()),
    );
    let js = rig.tenant_op_as(
        ActorKind::Human,
        &d1,
        &g1,
        "js",
        Mjudge::OP_TYPE,
        Mjudge::Supersede {
            target: i.op_hash(),
            replacement: r.op_hash(),
            policy: wf_polref(),
            reason: None,
        }
        .to_value(),
        1,
        None,
    );
    let ja = rig.tenant_op_as(
        ActorKind::Human,
        &d1,
        &g1,
        "ja",
        Mjudge::OP_TYPE,
        judge_basic(BasicVerdict::Accept, r.op_hash(), wf_polref()),
        2,
        Some(js.op_hash()),
    );
    let jx = with_revival.then(|| {
        rig.tenant_op_as(
            ActorKind::Human,
            &d1,
            &g1,
            "jx",
            Mjudge::OP_TYPE,
            judge_basic(BasicVerdict::Retract, r.op_hash(), wf_polref()),
            3,
            Some(ja.op_hash()),
        )
    });
    let c1 = rig.genesis_op.clone();
    let mut item_list: Vec<(&str, &crate::shapes::envelope::Signedop)> = vec![
        ("c1", &c1),
        ("c2", &c2),
        ("i", &i),
        ("r", &r),
        ("js", &js),
        ("ja", &ja),
    ];
    let (delivery, derived) = if let Some(jx) = &jx {
        item_list.push(("jx", jx));
        (
            json!(["c1", "c2", "i", "r", "js", "ja", "jx"]),
            json!([
                { "item": "i", "value": "candidate" },
                { "item": "r", "value": "retired" },
            ]),
        )
    } else {
        (
            json!(["c1", "c2", "i", "r", "js", "ja"]),
            json!([
                { "item": "i", "value": "superseded" },
                { "item": "r", "value": "accepted" },
            ]),
        )
    };
    status_vector(name, rig, &item_list, delivery, derived, Json::Null)
}

pub fn f11_status_superseded() -> Vector {
    supersede_rig("status-superseded-by-accepted-replacement", false)
}

pub fn f11_status_revival_on_replacement_loss() -> Vector {
    supersede_rig("status-revival-on-replacement-loss", true)
}

/// The polref mismatch (§13.3: policy hash mismatch → policy-missing)
/// — a fold vector: the judgment cites workflow-v1's id with a wrong
/// hash and pends.
pub fn f11_policy_hash_mismatch() -> Vector {
    let name = "judge-policy-hash-mismatch-pends";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let g1 = rig.genesis_grant.clone();
    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let i = rig.claim(&dev2, &grant2, "i", "a claim awaiting judgment", 1, None);
    let bad_pol = Polref {
        id: "workflow-v1".into(),
        version: 1,
        hash: [0x5a; 32],
    };
    let j = rig.tenant_op_as(
        ActorKind::Human,
        &d1,
        &g1,
        "j",
        Mjudge::OP_TYPE,
        judge_basic(BasicVerdict::Accept, i.op_hash(), bad_pol),
        1,
        None,
    );
    let c1 = rig.genesis_op.clone();
    let mut inputs = JsonMap::new();
    inputs.insert(
        "items".into(),
        items(&[("c1", &c1), ("c2", &c2), ("i", &i), ("j", &j)]),
    );
    inputs.insert(
        "deliveries".into(),
        json!([["c1", "c2", "i", "j"], ["j", "i", "c2", "c1"]]),
    );
    Vector {
        family: 11,
        name: name.into(),
        case_kind: "fold".into(),
        source: "11.2".into(),
        surfaces: vec!["core".into()],
        rng: Some(rig.rng.into_json()),
        inputs,
        expected: Expected::Result(json!({
            "per_item": [
                admits("c1"),
                admits("c2"),
                admits("i"),
                { "item": "j", "outcome": "policy-missing", "disposition": "pending-dependency" },
            ],
            "converge": true,
        })),
    }
}

/// Family 11 status vectors, in burn-down order.
pub fn corpus_status() -> Vec<Vector> {
    vec![
        f11_status_owner_accept(),
        f11_status_safe_human_dispute_counts(),
        f11_status_dispute_recorded_not_counting(),
        f11_status_author_retract(),
        f11_status_superseded(),
        f11_status_revival_on_replacement_loss(),
        f11_policy_hash_mismatch(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_status_corpus_matches_builders() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vectors");
        for v in corpus_status() {
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
    fn status_corpus_checks_clean() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let companion: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("d0a-vector-cases.v1.json")).unwrap(),
        )
        .unwrap();
        for v in corpus_status() {
            crate::vector::check(&v.to_json(), &companion)
                .unwrap_or_else(|e| panic!("{}: {e}", v.name));
        }
    }
}
