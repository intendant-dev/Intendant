//! The corpus's fold-lane vectors, families 7/10/11 (§13.3): control
//! admission negatives (arm/signature/body-hash/replay), the D-92
//! issuance gate, the one-live-compound rule, tenant chain and
//! actor-mint negatives, and the D-78 portable epoch currency —
//! each a real PlaneRig ceremony with byte-honest tampering where a
//! negative needs it (the typed layer cannot express an invalid
//! signature; a cloned triple with a mutated field can).

use crate::cbor;
use crate::shapes::control::Cgrant;
use crate::shapes::envelope::ActorKind;
use crate::shapes::identity::{Authproof, GrantTenant};
use crate::shapes::memory::{Mclaim, Merasereq};
use crate::shapes::{Class, Kind, ToValue, Verb};
use crate::tranche::{admits, draw_id, h_cert, h_grant, items, PlaneRig, TenantOverrides};
use crate::vector::{Expected, Vector};
use serde_json::{json, Map as JsonMap, Value as Json};

fn fold_vector(
    family: u8,
    name: &str,
    source: &str,
    rig: PlaneRig,
    item_list: &[(&str, &crate::shapes::envelope::Signedop)],
    deliveries: Json,
    expected: Json,
) -> Vector {
    let mut inputs = JsonMap::new();
    inputs.insert("items".into(), items(item_list));
    inputs.insert("deliveries".into(), deliveries);
    Vector {
        family,
        name: name.into(),
        case_kind: "fold".into(),
        source: source.into(),
        surfaces: vec!["core".into()],
        rng: Some(rig.rng.into_json()),
        inputs,
        expected: Expected::Result(expected),
    }
}

fn rejected(item: &str, outcome: &str, disposition: &str) -> Json {
    json!({ "item": item, "outcome": outcome, "disposition": disposition })
}

/// The §10.4 authorization-scope negatives, one per axis (the
/// D-203-ratified cheap-gap batch): every rejected item is a dev2
/// gen-1/seq-1 op with a distinct request_id — rejected ops never
/// advance the chain, so the axes probe independently. `xu` is a
/// type outside the closed §7.1/§11.1 registry (`op-unknown`); `xf`
/// is a release under a flowless grant (`no-flow`, the D-76/§11.8
/// flow match).
pub fn f11_scope_negatives() -> Vector {
    let name = "f11-scope-negatives";
    let mut rig = PlaneRig::new(name);
    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let dev3 = rig.mint_device("dev3");
    let grant3 = rig.simple_grant("grant3", &dev3, vec![Verb::Propose]);
    let c3 = rig.enroll_new(&dev3, vec![grant3.clone()], "wrap.dev3.eph");
    // A kinds-restricted and a tenant-restricted grant for dev2.
    let mut grant2k = rig.simple_grant("grant2k", &dev2, vec![Verb::Propose]);
    grant2k.kinds = Some(vec![Kind::Decision]);
    let g4 = rig.grant_op(grant2k.clone());
    let mut grant2t = rig.simple_grant("grant2t", &dev2, vec![Verb::Propose]);
    grant2t.tenants = vec![GrantTenant::Agenda];
    let g5 = rig.grant_op(grant2t.clone());
    // An export-verbed grant WITHOUT a flow (the §11.8/D-76 match
    // needs the verb to pass first — scope-op precedes no-flow).
    let grant2f = rig.simple_grant("grant2f", &dev2, vec![Verb::Export]);
    let g6 = rig.grant_op(grant2f.clone());

    // The one ADMITTED dev2 claim (seq 1): the chain-stage anchor for
    // the post-preamble negatives and the release's held source.
    let i0 = rig.claim(&dev2, &grant2, "i0", "the exportable source", 1, None);
    // scope-zone: the op's header zone is outside the grant's.
    let other_zone = draw_id(&mut rig.rng, "other.zone_id");
    let home = rig.home_space;
    let xz = rig.tenant_op_in(
        other_zone,
        home,
        ActorKind::Daemon,
        &dev2,
        &grant2,
        "xz",
        Mclaim::OP_TYPE,
        claim_body("the zone axis"),
        1,
        None,
    );
    // scope-space: the audit space is outside grant2's [home].
    let audit = rig.audit_space;
    let xs = rig.claim_in_space(&dev2, &grant2, audit, "xs", "the space axis", 1, None);
    // scope-op: an erase request under a propose-only grant.
    let xo = rig.tenant_op_as(
        ActorKind::Human,
        &dev2,
        &grant2,
        "xo",
        Merasereq::OP_TYPE,
        Merasereq {
            targets: vec![[0x11; 32]],
        }
        .to_value(),
        1,
        None,
    );
    // scope-kind: an observation under the Decision-kinded grant —
    // the kind check is post-preamble, so xk signs the advanced
    // chain position atop i0.
    let xk = rig.claim(
        &dev2,
        &grant2k,
        "xk",
        "the kind axis",
        2,
        Some(i0.op_hash()),
    );
    // scope-tenant: a memory op under the agenda-tenant grant.
    let xt = rig.claim(&dev2, &grant2t, "xt", "the tenant axis", 1, None);
    // no-grant: dev2 cites dev3's (held) grant.
    let xg = rig.claim(&dev2, &grant3, "xg", "the subject axis", 1, None);
    // op-unknown: a type outside the registry.
    let xu = rig.tenant_op_as(
        ActorKind::Daemon,
        &dev2,
        &grant2,
        "xu",
        "m.frobnicate",
        cbor::map(vec![]),
        1,
        None,
    );
    // no-flow: a release citing a HELD claim (sources must resolve
    // before the flow match) under the export-verbed flowless grant.
    let export_id = draw_id(&mut rig.rng, "rel.export_id");
    let digest = rig.rng.draw32("rel.content_digest");
    let dz = draw_id(&mut rig.rng, "dest.zone_id");
    let ds = draw_id(&mut rig.rng, "dest.space_id");
    let df = rig.rng.draw32("rel.data_frontier");
    let xf = rig.release_op_signed(
        &dev2,
        &grant2f,
        "xf",
        export_id,
        vec![i0.op_hash()],
        digest,
        dz,
        ds,
        df,
        2,
        Some(i0.op_hash()),
    );

    let c1 = rig.genesis_op.clone();
    let item_list: Vec<(&str, &crate::shapes::envelope::Signedop)> = vec![
        ("c1", &c1),
        ("c2", &c2),
        ("c3", &c3),
        ("g4", &g4),
        ("g5", &g5),
        ("g6", &g6),
        ("i0", &i0),
        ("xz", &xz),
        ("xs", &xs),
        ("xo", &xo),
        ("xk", &xk),
        ("xt", &xt),
        ("xg", &xg),
        ("xu", &xu),
        ("xf", &xf),
    ];
    let forward = json!([
        "c1", "c2", "c3", "g4", "g5", "g6", "i0", "xz", "xs", "xo", "xk", "xt", "xg", "xu", "xf"
    ]);
    let reversed = json!([
        "xf", "xu", "xg", "xt", "xk", "xo", "xs", "xz", "i0", "g6", "g5", "g4", "c3", "c2", "c1"
    ]);
    fold_vector(
        11,
        "scope-negatives-per-axis",
        "10.4",
        rig,
        &item_list,
        json!([forward, reversed]),
        json!({
            "per_item": [
                admits("c1"),
                admits("c2"),
                admits("c3"),
                admits("g4"),
                admits("g5"),
                admits("g6"),
                admits("i0"),
                rejected("xz", "scope-zone", "reject-permanent"),
                rejected("xs", "scope-space", "reject-permanent"),
                rejected("xo", "scope-op", "reject-permanent"),
                rejected("xk", "scope-kind", "reject-permanent"),
                rejected("xt", "scope-tenant", "reject-permanent"),
                rejected("xg", "no-grant", "reject-permanent"),
                rejected("xu", "op-unknown", "reject-permanent"),
                rejected("xf", "no-flow", "reject-permanent"),
            ],
            "converge": true,
        }),
    )
}

/// The ratified P1 profile's generation fail-close: a gen-2 writer
/// quarantines `lineage-gen` (revivable if a later profile
/// implements the generation machine — the D-140 below-bound
/// reading).
pub fn f11_second_generation_fail_closed() -> Vector {
    let name = "f11-gen2-fail-closed";
    let mut rig = PlaneRig::new(name);
    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let home = rig.home_space;
    let gz = rig.zone_id;
    let x = rig.tenant_op_over(
        gz,
        home,
        ActorKind::Daemon,
        &dev2,
        &grant2,
        "x",
        Mclaim::OP_TYPE,
        claim_body("a second-generation write"),
        1,
        None,
        TenantOverrides {
            actor_id: None,
            capability_epoch: 1,
            authored_kek_epoch: 1,
            attested_by: None,
            writer_gen: Some(2),
        },
    );
    let c1 = rig.genesis_op.clone();
    fold_vector(
        11,
        "second-generation-fail-closed",
        "9.3",
        rig,
        &[("c1", &c1), ("c2", &c2), ("x", &x)],
        json!([["c1", "c2", "x"], ["x", "c2", "c1"]]),
        json!({
            "per_item": [
                admits("c1"),
                admits("c2"),
                rejected("x", "lineage-gen", "quarantine-reproposal"),
            ],
            "converge": true,
        }),
    )
}

/// A minimal claim body for the negative axes.
fn claim_body(statement: &str) -> cbor::Value {
    Mclaim {
        kind: Kind::Observation,
        statement: statement.into(),
        sensitivity: Class::Private,
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
    }
    .to_value()
}

// --------------------------------------------------------- family 7

/// D-92: issuance to a device whose `revocation_id` is REVOKED
/// rejects. dev2 enrolls bare (no grants, no wraps — zero-authorship,
/// zero-wrap), so the exclude compound completes IMMEDIATELY at its
/// own acceptance (empty cutoffs total over an empty authorship
/// domain; the decryptable-wrap domain is empty). The late grant
/// dies; the control chain makes the order canonical (the fresh fold
/// pends the grant `causal-missing` until the compound lands, then
/// derives the same rejection).
pub fn f7_issuance_to_revoked_device() -> Vector {
    let name = "issuance-to-revoked-device-rejects";
    let mut rig = PlaneRig::new(name);
    let dev2 = rig.mint_device("dev2");
    let c2 = rig.enroll_new_with_wraps(&dev2, vec![], vec![]);
    let r = rig.revoke_device_exclude(&dev2, vec![]);
    let g = {
        let grant = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
        rig.grant_op(grant)
    };
    let c1 = rig.genesis_op.clone();
    fold_vector(
        7,
        name,
        "7.1",
        rig,
        &[("c1", &c1), ("c2", &c2), ("r", &r), ("g", &g)],
        json!([["c1", "c2", "r", "g"], ["c1", "c2", "g", "r"]]),
        json!({
            "per_item": [
                admits("c1"),
                admits("c2"),
                admits("r"),
                rejected("g", "body-invariant", "reject-permanent"),
            ],
            "converge": true,
        }),
    )
}

/// At most one live compound per `revocation_id`: dev2 holds an
/// epoch-1 wrap, so the first compound pends (nonempty wrap domain)
/// and RESERVES; the second — fresh bytes, same target — is
/// `body-invariant` while the first lives.
pub fn f7_second_live_compound() -> Vector {
    let name = "second-live-compound-rejects";
    let mut rig = PlaneRig::new(name);
    let dev2 = rig.mint_device("dev2");
    let c2 = rig.enroll_new(&dev2, vec![], "wrap.dev2.eph");
    let r1 = rig.revoke_device_exclude(&dev2, vec![]);
    let r2 = rig.revoke_device_exclude(&dev2, vec![]);
    let c1 = rig.genesis_op.clone();
    fold_vector(
        7,
        name,
        "7.1",
        rig,
        &[("c1", &c1), ("c2", &c2), ("r1", &r1), ("r2", &r2)],
        json!([["c1", "c2", "r1", "r2"], ["c1", "c2", "r2", "r1"]]),
        json!({
            "per_item": [
                admits("c1"),
                admits("c2"),
                { "item": "r1", "outcome": "ref-unresolved", "disposition": "pending-dependency" },
                rejected("r2", "body-invariant", "reject-permanent"),
            ],
            "converge": true,
        }),
    )
}

/// An arm-valid header over a tampered signature: `sig-invalid`,
/// before any body question (the §4.5 explicit sig stage).
pub fn f7_control_sig_tamper() -> Vector {
    let name = "control-signature-tamper";
    let mut rig = PlaneRig::new(name);
    let dev2 = rig.mint_device("dev2");
    let mut c2 = rig.enroll_new(&dev2, vec![], "wrap.dev2.eph");
    c2.signature[0] ^= 1;
    let c1 = rig.genesis_op.clone();
    fold_vector(
        7,
        name,
        "4.5",
        rig,
        &[("c1", &c1), ("c2", &c2)],
        json!([["c1", "c2"], ["c2", "c1"]]),
        json!({
            "per_item": [
                admits("c1"),
                rejected("c2", "sig-invalid", "reject-permanent"),
            ],
            "converge": true,
        }),
    )
}

/// A valid signature over a substituted body: the header's
/// `body_hash` no longer matches — `body-hash` (the signature covers
/// the header alone; the body binds through it, O1).
pub fn f7_control_body_tamper() -> Vector {
    let name = "control-body-tamper";
    let mut rig = PlaneRig::new(name);
    let dev2 = rig.mint_device("dev2");
    let mut c2 = rig.enroll_new(&dev2, vec![], "wrap.dev2.eph");
    c2.body = cbor::map(vec![("swapped", crate::shapes::u(1))]);
    let c1 = rig.genesis_op.clone();
    fold_vector(
        7,
        name,
        "4.5",
        rig,
        &[("c1", &c1), ("c2", &c2)],
        json!([["c1", "c2"], ["c2", "c1"]]),
        json!({
            "per_item": [
                admits("c1"),
                rejected("c2", "body-hash", "reject-permanent"),
            ],
            "converge": true,
        }),
    )
}

/// A control op sealed under the DEV arm (root-signed, but the wrong
/// authority shape): `proof-arm`.
pub fn f7_wrong_proof_arm() -> Vector {
    let name = "control-wrong-proof-arm";
    let mut rig = PlaneRig::new(name);
    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let proof = Authproof::Dev {
        cert: h_cert(&dev2.cert),
        cap: h_grant(&grant2),
    };
    let request_id = draw_id(&mut rig.rng, "ctrl2.request_id");
    let g = rig.seal_ctrl_with_request(
        Cgrant::OP_TYPE,
        proof,
        Cgrant { grant: grant2 }.to_value(),
        request_id,
    );
    let c1 = rig.genesis_op.clone();
    fold_vector(
        7,
        name,
        "7.1",
        rig,
        &[("c1", &c1), ("g", &g)],
        json!([["c1", "g"], ["g", "c1"]]),
        json!({
            "per_item": [
                admits("c1"),
                rejected("g", "proof-arm", "reject-permanent"),
            ],
            "converge": true,
        }),
    )
}

/// O5 replay, the fork half: differing bytes under one consumed
/// `request_id` → `request-fork` (surfaced as fork evidence).
pub fn f7_request_fork() -> Vector {
    let name = "consumed-request-id-fork";
    let mut rig = PlaneRig::new(name);
    let dev2 = rig.mint_device("dev2");
    let c2 = rig.enroll_new(&dev2, vec![], "wrap.dev2.eph");
    let grant_a = rig.simple_grant("grant-a", &dev2, vec![Verb::Propose]);
    let g1 = rig.grant_op(grant_a);
    let consumed = g1.header.request_id;
    let grant_b = rig.simple_grant("grant-b", &dev2, vec![Verb::Read]);
    let g2 = rig.seal_ctrl_with_request(
        Cgrant::OP_TYPE,
        Authproof::Admin {
            epoch: 1,
            ctrl_frontier: g1.op_hash(),
        },
        Cgrant { grant: grant_b }.to_value(),
        consumed,
    );
    let c1 = rig.genesis_op.clone();
    fold_vector(
        7,
        name,
        "11.1",
        rig,
        &[("c1", &c1), ("c2", &c2), ("g1", &g1), ("g2", &g2)],
        json!([["c1", "c2", "g1", "g2"], ["c2", "c1", "g1", "g2"]]),
        json!({
            "per_item": [
                admits("c1"),
                admits("c2"),
                admits("g1"),
                rejected("g2", "request-fork", "reject-permanent"),
            ],
            "converge": true,
        }),
    )
}

/// O5 replay, the idempotent half: byte-identical redelivery of an
/// accepted operation → `duplicate` (duplicate-idempotent) — the
/// items map carries the SAME bytes under two names.
pub fn f7_duplicate_idempotent() -> Vector {
    let name = "byte-identical-replay-duplicate";
    let mut rig = PlaneRig::new(name);
    let dev2 = rig.mint_device("dev2");
    let c2 = rig.enroll_new(&dev2, vec![], "wrap.dev2.eph");
    let c1 = rig.genesis_op.clone();
    fold_vector(
        7,
        name,
        "11.1",
        rig,
        &[("c1", &c1), ("c2", &c2), ("c2dup", &c2)],
        json!([["c1", "c2", "c2dup"], ["c2", "c1", "c2dup"]]),
        json!({
            "per_item": [
                admits("c1"),
                admits("c2"),
                { "item": "c2dup", "outcome": "duplicate", "disposition": "duplicate-idempotent" },
            ],
            "converge": true,
        }),
    )
}

// -------------------------------------------------------- family 11

/// O8 actor-id minting: a daemon actor whose id is not the writing
/// device's hex id → `body-invariant`.
pub fn f11_actor_id_mint() -> Vector {
    let name = "actor-id-mint-negative";
    let mut rig = PlaneRig::new(name);
    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let (gz, home) = (rig.zone_id, rig.home_space);
    let body = Mclaim {
        kind: Kind::Observation,
        statement: "actor identity is minted, never chosen".into(),
        sensitivity: Class::Private,
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
    let i = rig.tenant_op_over(
        gz,
        home,
        ActorKind::Daemon,
        &dev2,
        &grant2,
        "i",
        Mclaim::OP_TYPE,
        body.to_value(),
        1,
        None,
        TenantOverrides {
            actor_id: Some("deadbeef".into()),
            ..TenantOverrides::default()
        },
    );
    let c1 = rig.genesis_op.clone();
    fold_vector(
        11,
        name,
        "10.1",
        rig,
        &[("c1", &c1), ("c2", &c2), ("i", &i)],
        json!([["c1", "c2", "i"], ["i", "c2", "c1"]]),
        json!({
            "per_item": [
                admits("c1"),
                admits("c2"),
                rejected("i", "body-invariant", "reject-permanent"),
            ],
            "converge": true,
        }),
    )
}

// -------------------------------------------------------- family 10

/// D-93: the grant-epoch lower bound — an epoch-5 grant admits at
/// issuance, but an epoch-1 operation citing it is behind the
/// grant's signed epoch → `capability-epoch` (quarantine-reproposal,
/// revivable when the epoch opens).
pub fn f10_grant_epoch_lower_bound() -> Vector {
    let name = "grant-epoch-lower-bound";
    let mut rig = PlaneRig::new(name);
    let dev2 = rig.mint_device("dev2");
    let mut grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    grant2.capability_epoch = 5;
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let i = rig.claim(&dev2, &grant2, "i", "written before its epoch", 1, None);
    let c1 = rig.genesis_op.clone();
    fold_vector(
        10,
        name,
        "4.3",
        rig,
        &[("c1", &c1), ("c2", &c2), ("i", &i)],
        json!([["c1", "c2", "i"], ["i", "c2", "c1"]]),
        json!({
            "per_item": [
                admits("c1"),
                admits("c2"),
                rejected("i", "capability-epoch", "quarantine-reproposal"),
            ],
            "converge": true,
        }),
    )
}

/// D-78 portable currency: an operation signed at capability epoch 2
/// pends `epoch-unopened` until the chain opens the epoch (the
/// `c.cap_epoch_bump` with total strict coverage), then admits —
/// on both delivery orders and the fresh fold.
pub fn f10_epoch_unopened_converges() -> Vector {
    let name = "epoch-unopened-pends-until-the-bump";
    let mut rig = PlaneRig::new(name);
    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let (gz, home) = (rig.zone_id, rig.home_space);
    let body = Mclaim {
        kind: Kind::Observation,
        statement: "signed into the next capability window".into(),
        sensitivity: Class::Private,
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
    let i = rig.tenant_op_over(
        gz,
        home,
        ActorKind::Daemon,
        &dev2,
        &grant2,
        "i",
        Mclaim::OP_TYPE,
        body.to_value(),
        1,
        None,
        TenantOverrides {
            capability_epoch: 2,
            ..TenantOverrides::default()
        },
    );
    // The bump's strict union coverage: every live lineage (dev1's —
    // the genesis + audit grants — and dev2's), empty heads (no
    // accepted tenant ops at the bump's position on either order).
    let b = {
        let fc = |lineage| crate::shapes::Frontierclose {
            zone_id: gz,
            lineage,
            heads: vec![],
        };
        let (l1, l2) = (rig.dev1.lineage, dev2.lineage);
        rig.epoch_bump(2, vec![fc(l1), fc(l2)])
    };
    let c1 = rig.genesis_op.clone();
    fold_vector(
        10,
        name,
        "7.4",
        rig,
        &[("c1", &c1), ("c2", &c2), ("i", &i), ("b", &b)],
        json!([["c1", "c2", "i", "b"], ["c1", "c2", "b", "i"]]),
        json!({
            "per_item": [
                admits("c1"),
                admits("c2"),
                admits("i"),
                admits("b"),
            ],
            "converge": true,
            "trace": [{
                "delivery": 0,
                "after": "i",
                "item": "i",
                "outcome": "epoch-unopened",
                "disposition": "pending-dependency",
            }],
        }),
    )
}

/// §9.3 fork: two different operations at one tenant chain position
/// — the second is fork evidence, `freeze-writer`.
pub fn f10_tenant_fork() -> Vector {
    let name = "tenant-same-seq-fork";
    let mut rig = PlaneRig::new(name);
    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let i1 = rig.claim(&dev2, &grant2, "i1", "the first seq-one claim", 1, None);
    let i2 = rig.claim(&dev2, &grant2, "i2", "a competing seq-one claim", 1, None);
    let c1 = rig.genesis_op.clone();
    fold_vector(
        10,
        name,
        "9.3",
        rig,
        &[("c1", &c1), ("c2", &c2), ("i1", &i1), ("i2", &i2)],
        json!([["c1", "c2", "i1", "i2"], ["c2", "c1", "i1", "i2"]]),
        json!({
            "per_item": [
                admits("c1"),
                admits("c2"),
                admits("i1"),
                rejected("i2", "fork", "freeze-writer"),
            ],
            "converge": true,
        }),
    )
}

/// §9.3 gap: the seq-2 claim delivered first pends `causal-missing`
/// and admits when seq 1 arrives — both orders converge.
pub fn f10_causal_missing_converges() -> Vector {
    let name = "tenant-gap-pends-causal-missing";
    let mut rig = PlaneRig::new(name);
    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let i1 = rig.claim(&dev2, &grant2, "i1", "the opening claim", 1, None);
    let i2 = rig.claim(
        &dev2,
        &grant2,
        "i2",
        "the successor claim",
        2,
        Some(i1.op_hash()),
    );
    let c1 = rig.genesis_op.clone();
    fold_vector(
        10,
        name,
        "9.3",
        rig,
        &[("c1", &c1), ("c2", &c2), ("i1", &i1), ("i2", &i2)],
        json!([["c1", "c2", "i2", "i1"], ["c1", "c2", "i1", "i2"]]),
        json!({
            "per_item": [
                admits("c1"),
                admits("c2"),
                admits("i1"),
                admits("i2"),
            ],
            "converge": true,
            "trace": [{
                "delivery": 0,
                "after": "i2",
                "item": "i2",
                "outcome": "causal-missing",
                "disposition": "pending-dependency",
            }],
        }),
    )
}

/// The fold-lane corpus vectors, family-ordered.
pub fn corpus_fold() -> Vec<Vector> {
    vec![
        f7_issuance_to_revoked_device(),
        f7_second_live_compound(),
        f7_control_sig_tamper(),
        f7_control_body_tamper(),
        f7_wrong_proof_arm(),
        f7_request_fork(),
        f7_duplicate_idempotent(),
        f10_grant_epoch_lower_bound(),
        f10_epoch_unopened_converges(),
        f10_tenant_fork(),
        f10_causal_missing_converges(),
        f11_actor_id_mint(),
        f11_scope_negatives(),
        f11_second_generation_fail_closed(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift gate: committed corpus-fold vectors byte-match their
    /// builders.
    #[test]
    fn committed_fold_corpus_matches_builders() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vectors");
        for v in corpus_fold() {
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
    fn fold_corpus_checks_clean() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let companion: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("d0a-vector-cases.v1.json")).unwrap(),
        )
        .unwrap();
        for v in corpus_fold() {
            crate::vector::check(&v.to_json(), &companion)
                .unwrap_or_else(|e| panic!("{}: {e}", v.name));
        }
    }
}
