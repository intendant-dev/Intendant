//! Corpus family 10 budgets (§4.3): per-`(grant_id, lineage)` window
//! accounting as a pure fold in canonical `(gen, seq)` order over
//! the ACCEPTED set — a late-arriving earlier operation
//! deterministically displaces later-ordered operations past the
//! budget line (D-86/D-94); windows reset ONLY on an admin
//! `c.cap_epoch_bump` (a `c.zone_policy` epoch advance re-arms
//! nothing — D-79).
//!
//! - `budget-exhaustion-canonical-displacement`: `max_ops = 2`,
//!   three claims — the canonically-last claim quarantines
//!   `(budget, quarantine-reproposal)` on EVERY delivery order,
//!   including the one where it arrived first and was admitted
//!   (displacement is a derived state).
//! - `budget-window-reset-on-bump`: window 0 exhausts; the bump
//!   opens window 1 at epoch 2; the epoch-2 claim admits; the next
//!   epoch-2 claim re-exhausts.
//! - `budget-zone-policy-rearms-nothing` (D-79): the same shape with
//!   `c.zone_policy` advancing the epoch — the epoch-2 claim stays
//!   in window 0 and quarantines.

use crate::shapes::envelope::Signedop;
use crate::shapes::identity::Budget;
use crate::shapes::{DeadlineFallback, Frontierclose, Strictness, Verb, Zonepolicy};
use crate::tranche::{items, PlaneRig, TenantOverrides};
use crate::vector::{Expected, Vector};
use serde_json::{json, Map as JsonMap, Value as Json};

fn budget_vector(
    name: &str,
    rig: PlaneRig,
    item_list: &[(&str, &Signedop)],
    deliveries: Json,
    per_item: Json,
) -> Vector {
    let mut inputs = JsonMap::new();
    inputs.insert("items".into(), items(item_list));
    inputs.insert("deliveries".into(), deliveries);
    Vector {
        family: 10,
        name: name.into(),
        case_kind: "fold".into(),
        source: "4.3".into(),
        surfaces: vec!["core".into()],
        rng: Some(rig.rng.into_json()),
        inputs,
        expected: Expected::Result(json!({
            "per_item": per_item,
            "converge": true,
        })),
    }
}

/// A tight-budget grant on the genesis plane.
fn tight_grant(rig: &mut PlaneRig, max_ops: u64) -> (crate::shapes::identity::Grant, Signedop) {
    let d1 = rig.dev1.clone();
    let (z, home) = (rig.zone_id, rig.home_space);
    let mut g = rig.grant_in("grantbudget", &d1, vec![Verb::Propose], z, vec![home]);
    g.budget = Some(Budget {
        max_ops,
        max_bytes: 1_048_576,
    });
    let op = rig.grant_op(g.clone());
    (g, op)
}

/// Three claims against `max_ops = 2`: the canonically-last one
/// displaces on every order.
pub fn f10_budget_displacement() -> Vector {
    let name = "f10-budget-displacement";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let (g, c2) = tight_grant(&mut rig, 2);
    let i1 = rig.claim(&d1, &g, "i1", "first entry under the tight budget", 1, None);
    let i2 = rig.claim(
        &d1,
        &g,
        "i2",
        "second entry fills the window",
        2,
        Some(i1.op_hash()),
    );
    let i3 = rig.claim(
        &d1,
        &g,
        "i3",
        "third entry crosses the line",
        3,
        Some(i2.op_hash()),
    );
    let c1 = rig.genesis_op.clone();
    budget_vector(
        name,
        rig,
        &[
            ("c1", &c1),
            ("c2", &c2),
            ("i1", &i1),
            ("i2", &i2),
            ("i3", &i3),
        ],
        // The second order delivers i3 FIRST: it admits, then the
        // earlier-ordered arrivals displace it (D-86).
        json!([
            ["c1", "c2", "i1", "i2", "i3"],
            ["c1", "c2", "i3", "i1", "i2"],
        ]),
        json!([
            { "item": "c1" },
            { "item": "c2" },
            { "item": "i1" },
            { "item": "i2" },
            { "item": "i3", "outcome": "budget", "disposition": "quarantine-reproposal" },
        ]),
    )
}

/// The bump opens a fresh window (budgets reset on
/// `c.cap_epoch_bump` ONLY).
pub fn f10_budget_window_reset_on_bump() -> Vector {
    let name = "f10-budget-bump-reset";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let (g, c2) = tight_grant(&mut rig, 1);
    let i1 = rig.claim(&d1, &g, "i1", "window zero fills", 1, None);
    let fc = Frontierclose {
        zone_id: rig.zone_id,
        lineage: d1.lineage,
        heads: vec![],
    };
    let c3 = rig.epoch_bump(2, vec![fc]);
    let over = TenantOverrides {
        actor_id: None,
        capability_epoch: 2,
        authored_kek_epoch: 1,
    };
    let i2 = rig.claim_over(
        &d1,
        &g,
        "i2",
        "window one opens",
        2,
        Some(i1.op_hash()),
        over,
    );
    let over3 = TenantOverrides {
        actor_id: None,
        capability_epoch: 2,
        authored_kek_epoch: 1,
    };
    let i3 = rig.claim_over(
        &d1,
        &g,
        "i3",
        "window one fills too",
        3,
        Some(i2.op_hash()),
        over3,
    );
    let c1 = rig.genesis_op.clone();
    budget_vector(
        name,
        rig,
        &[
            ("c1", &c1),
            ("c2", &c2),
            ("c3", &c3),
            ("i1", &i1),
            ("i2", &i2),
            ("i3", &i3),
        ],
        json!([
            ["c1", "c2", "i1", "c3", "i2", "i3"],
            ["i3", "i2", "c3", "i1", "c2", "c1"],
        ]),
        json!([
            { "item": "c1" },
            { "item": "c2" },
            { "item": "c3" },
            { "item": "i1" },
            { "item": "i2" },
            { "item": "i3", "outcome": "budget", "disposition": "quarantine-reproposal" },
        ]),
    )
}

/// D-79: a `c.zone_policy` advance re-arms nothing — the epoch-2
/// claim stays in the exhausted window.
pub fn f10_budget_zone_policy_rearms_nothing() -> Vector {
    let name = "f10-budget-policy-noreset";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let (g, c2) = tight_grant(&mut rig, 1);
    let i1 = rig.claim(&d1, &g, "i1", "window zero fills", 1, None);
    let policy = Zonepolicy {
        zone_id: rig.zone_id,
        strictness: Strictness::Strict,
        deadline_fallback: DeadlineFallback::Budgets,
        require_cert_deadlines: false,
        grant_epoch_slack: None,
        time_witnesses: None,
        connect_service_key: None,
    };
    let fc = Frontierclose {
        zone_id: rig.zone_id,
        lineage: d1.lineage,
        heads: vec![],
    };
    let c3 = rig.zone_policy_op(policy, vec![fc]);
    let over = TenantOverrides {
        actor_id: None,
        capability_epoch: 2,
        authored_kek_epoch: 1,
    };
    let i2 = rig.claim_over(
        &d1,
        &g,
        "i2",
        "the same window binds",
        2,
        Some(i1.op_hash()),
        over,
    );
    let c1 = rig.genesis_op.clone();
    budget_vector(
        name,
        rig,
        &[
            ("c1", &c1),
            ("c2", &c2),
            ("c3", &c3),
            ("i1", &i1),
            ("i2", &i2),
        ],
        json!([
            ["c1", "c2", "i1", "c3", "i2"],
            ["i2", "c3", "i1", "c2", "c1"],
        ]),
        json!([
            { "item": "c1" },
            { "item": "c2" },
            { "item": "c3" },
            { "item": "i1" },
            { "item": "i2", "outcome": "budget", "disposition": "quarantine-reproposal" },
        ]),
    )
}

pub fn corpus_budget() -> Vec<Vector> {
    vec![
        f10_budget_displacement(),
        f10_budget_window_reset_on_bump(),
        f10_budget_zone_policy_rearms_nothing(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_budget_corpus_matches_builders() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vectors");
        for v in corpus_budget() {
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
    fn budget_corpus_checks_clean() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let companion: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("d0a-vector-cases.v1.json")).unwrap(),
        )
        .unwrap();
        for v in corpus_budget() {
            crate::vector::check(&v.to_json(), &companion)
                .unwrap_or_else(|e| panic!("{}: {e}", v.name));
        }
    }
}
