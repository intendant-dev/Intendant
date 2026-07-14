//! Corpus family 9 (§9.1 acceptance deadlines + §4.7/T5 leases) —
//! the receipts engine's scoped opening: qualified `accept` receipts
//! admit deadline-bearing writes; `deadline-unreceipted` and
//! `lease-missing` pend; a held out-of-window receipt under a valid
//! lease is `lease-stale` (quarantine-reproposal); T2 excludes the
//! operation's own signer; a witnessless zone (the B.1 genesis
//! posture) can never qualify time evidence. The witness plane rides
//! a `c.zone_policy` install (D-69: acceptance advances the
//! capability epoch), which is also the engine's first policy-change
//! vector.
//!
//! Fixture conventions (register entries):
//! - Receipts, leases, and the §5.6 index ride `aux` — HELD context,
//!   not folded events: aux is state, so receipt-ARRIVAL dynamics are
//!   outside the fold lane (the pend vectors assert the
//!   without-evidence final state; the admit vectors the with-).
//! - aux name `index` = the §5.6 local `item_addr ↔ op` binding as a
//!   canonical CBOR array of `{item_addr, op}` maps — subjects stay
//!   item addresses per T1; the vector lane's binding is the index,
//!   exactly as on a real zone member. The claim's `item_addr` comes
//!   from really sealing the claim bytes under the family-14 KAT
//!   constants (DEK `[0x91;32]`, nonce `[0x92;12]`).
//! - Every other aux entry is a `Signed<…>` receipt or lease
//!   (discriminated by shape).
//! - `lease-missing` vs `lease-stale`: no held in-window evidence
//!   AND no receipt at all = `lease-missing` (awaiting arrival —
//!   pending-dependency); a held QUALIFIED receipt outside every
//!   valid lease window = `lease-stale` (conclusive staleness —
//!   quarantine-reproposal, per the §10.5 disposition split).

use crate::cbor;
use crate::keyschedule::{item_addr, seal_item};
use crate::shapes::envelope::Signedop;
use crate::shapes::{DeadlineFallback, Frontierclose, Strictness, TimeWitness, Verb, Zonepolicy};
use crate::tranche::{items, Device, PlaneRig, TenantOverrides, T0_MS};
use crate::vector::{Expected, Vector};
use serde_json::{json, Map as JsonMap, Value as Json};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The deadline every deadline-bearing grant here carries.
const DEADLINE: u64 = T0_MS + 5 * 86_400_000;

/// The §5.6 index aux entry: `[{item_addr, op}]`, canonical CBOR.
fn index_aux(entries: &[([u8; 32], [u8; 32])]) -> Vec<u8> {
    let arr = cbor::Value::Array(
        entries
            .iter()
            .map(|(addr, op)| {
                cbor::map(vec![
                    ("item_addr", cbor::Value::Bytes(addr.to_vec())),
                    ("op", cbor::Value::Bytes(op.to_vec())),
                ])
            })
            .collect(),
    );
    cbor::encode(&arr).expect("index encodes")
}

/// The claim's storage address under the family-14 KAT constants.
fn claim_addr(rig: &PlaneRig, claim: &Signedop) -> [u8; 32] {
    let core = seal_item(
        &[0x91; 32],
        [0x92; 12],
        &rig.plane_id,
        &rig.zone_id,
        &claim.encode(),
    );
    item_addr(&core)
}

/// A witness plane: dev2 enrolled (Propose grant) and installed as
/// the zone's time witness by a `c.zone_policy` at capability
/// epoch 2 (strict zone — the advance carries closure coverage of
/// both live lineages).
struct WitnessRig {
    rig: PlaneRig,
    d1: Device,
    d2: Device,
    ops: Vec<(&'static str, Signedop)>,
}

fn witness_rig(name: &str) -> WitnessRig {
    let mut rig = PlaneRig::new(name);
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
    WitnessRig {
        rig,
        d1,
        d2,
        ops: vec![("c1", c1), ("c2", c2), ("c3", c3)],
    }
}

/// Assemble one family-9 fold vector: two delivery orders (forward +
/// reversed) and the fresh-fold converge standard.
fn time_vector(
    name: &str,
    rig: PlaneRig,
    item_list: &[(&str, &Signedop)],
    aux: &[(&str, Vec<u8>)],
    per_item: Json,
) -> Vector {
    let mut inputs = JsonMap::new();
    inputs.insert("items".into(), items(item_list));
    let mut aux_map = JsonMap::new();
    for (n, bytes) in aux {
        aux_map.insert((*n).into(), json!(hex(bytes)));
    }
    inputs.insert("aux".into(), Json::Object(aux_map));
    let forward: Vec<&str> = item_list.iter().map(|(n, _)| *n).collect();
    let mut reversed = forward.clone();
    reversed.reverse();
    inputs.insert("deliveries".into(), json!([forward, reversed]));
    Vector {
        family: 9,
        name: name.into(),
        case_kind: "fold".into(),
        source: "9.1".into(),
        surfaces: vec!["core".into()],
        rng: Some(rig.rng.into_json()),
        inputs,
        expected: Expected::Result(json!({
            "per_item": per_item,
            "converge": true,
        })),
    }
}

/// One deadline-bearing claim arc on the witness plane; returns the
/// rig, the item list, and the claim's (item_addr, op_hash).
struct DeadlineArc {
    rig: PlaneRig,
    d1: Device,
    d2: Device,
    ops: Vec<(&'static str, Signedop)>,
    addr: [u8; 32],
    claim_hash: [u8; 32],
}

fn deadline_arc(name: &str, online_lease: bool) -> DeadlineArc {
    let WitnessRig {
        mut rig,
        d1,
        d2,
        mut ops,
    } = witness_rig(name);
    let mut g = rig.grant_in("grantdl", &d1, vec![Verb::Propose], rig.zone_id, {
        let home = rig.home_space;
        vec![home]
    });
    g.capability_epoch = 2;
    if online_lease {
        g.online_lease = true;
        g.max_age_ms = Some(2 * 86_400_000);
    } else {
        g.expiry_deadline_ms = Some(DEADLINE);
    }
    let c4 = rig.grant_op(g.clone());
    let i = rig.claim_over(
        &d1,
        &g,
        "i",
        "the tide gauge reads four feet at the north pier",
        1,
        None,
        TenantOverrides {
            actor_id: None,
            capability_epoch: 2,
            authored_kek_epoch: 1,
            attested_by: None,
        },
    );
    let addr = claim_addr(&rig, &i);
    let claim_hash = i.op_hash();
    ops.push(("c4", c4));
    ops.push(("i", i));
    DeadlineArc {
        rig,
        d1,
        d2,
        ops,
        addr,
        claim_hash,
    }
}

fn refs<'a>(ops: &'a [(&'static str, Signedop)]) -> Vec<(&'static str, &'a Signedop)> {
    ops.iter().map(|(n, o)| (*n, o)).collect()
}

/// All control ops admit; the claim's row carries `pair`.
fn rows(ops: &[(&'static str, Signedop)], pair: Option<(&str, &str)>) -> Json {
    Json::Array(
        ops.iter()
            .map(|(n, _)| match pair {
                Some((o, d)) if *n == "i" => json!({ "item": n, "outcome": o, "disposition": d }),
                _ => json!({ "item": n }),
            })
            .collect(),
    )
}

/// Qualified witness receipt before the deadline: the claim admits.
pub fn f9_deadline_receipted_admits() -> Vector {
    let mut a = deadline_arc("f9-deadline-receipted", false);
    let (d2, addr) = (a.d2.clone(), a.addr);
    let rcpt = a.rig.accept_receipt(&d2, addr, T0_MS + 86_400_000);
    let aux = [
        ("index", index_aux(&[(a.addr, a.claim_hash)])),
        ("receipt.accept.d2", rcpt.encode()),
    ];
    let per_item = rows(&a.ops, None);
    time_vector(
        "deadline-receipted-admits",
        a.rig,
        &refs(&a.ops),
        &aux,
        per_item,
    )
}

/// No receipt at all: (deadline-unreceipted, pending-dependency).
pub fn f9_deadline_unreceipted_pends() -> Vector {
    let a = deadline_arc("f9-deadline-unreceipted", false);
    let aux = [("index", index_aux(&[(a.addr, a.claim_hash)]))];
    let per_item = rows(&a.ops, Some(("deadline-unreceipted", "pending-dependency")));
    time_vector(
        "deadline-unreceipted-pends",
        a.rig,
        &refs(&a.ops),
        &aux,
        per_item,
    )
}

/// T2 self-exclusion: the signer's own receipt never qualifies —
/// the claim stays pending with the receipt HELD.
pub fn f9_deadline_self_receipt_nonqualifying() -> Vector {
    let mut a = deadline_arc("f9-deadline-self-receipt", false);
    let (d1, addr) = (a.d1.clone(), a.addr);
    let rcpt = a.rig.accept_receipt(&d1, addr, T0_MS + 86_400_000);
    let aux = [
        ("index", index_aux(&[(a.addr, a.claim_hash)])),
        ("receipt.accept.self", rcpt.encode()),
    ];
    let per_item = rows(&a.ops, Some(("deadline-unreceipted", "pending-dependency")));
    time_vector(
        "deadline-self-receipt-nonqualifying",
        a.rig,
        &refs(&a.ops),
        &aux,
        per_item,
    )
}

/// The witnessless B.1 zone: no receipt can EVER qualify (the §9.1
/// unusable-by-construction lane) — dev2 is enrolled and receipts,
/// but the epoch-1 policy lists no witnesses.
pub fn f9_witnessless_zone_deadline_unusable() -> Vector {
    let name = "f9-witnessless-deadline";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let d2 = rig.mint_device("dev2");
    let g2 = rig.simple_grant("grant2", &d2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&d2, vec![g2], "wrap.dev2.eph");
    let mut g = {
        let (z, home) = (rig.zone_id, rig.home_space);
        rig.grant_in("grantdl", &d1, vec![Verb::Propose], z, vec![home])
    };
    g.expiry_deadline_ms = Some(DEADLINE);
    let c3 = rig.grant_op(g.clone());
    let i = rig.claim(
        &d1,
        &g,
        "i",
        "the breakwater lamp burned through the night",
        1,
        None,
    );
    let addr = claim_addr(&rig, &i);
    let rcpt = rig.accept_receipt(&d2, addr, T0_MS + 3_600_000);
    let c1 = rig.genesis_op.clone();
    let ops = vec![("c1", c1), ("c2", c2), ("c3", c3), ("i", i.clone())];
    let aux = [
        ("index", index_aux(&[(addr, i.op_hash())])),
        ("receipt.accept.d2", rcpt.encode()),
    ];
    let per_item = rows(&ops, Some(("deadline-unreceipted", "pending-dependency")));
    time_vector(
        "witnessless-zone-deadline-unusable",
        rig,
        &refs(&ops),
        &aux,
        per_item,
    )
}

/// T5 both-legs: a valid lease for (grant_id, lineage) plus an
/// in-window qualified receipt — the online-lease claim admits.
pub fn f9_lease_online_grant_admits() -> Vector {
    let mut a = deadline_arc("f9-lease-admits", true);
    let gid = grant_id_of(&a.ops[3].1);
    let (d2, lineage, addr) = (a.d2.clone(), a.d1.lineage, a.addr);
    let lease = a
        .rig
        .lease_stmt(&d2, gid, lineage, T0_MS, T0_MS + 86_400_000);
    let rcpt = a.rig.accept_receipt(&d2, addr, T0_MS + 43_200_000);
    let aux = [
        ("index", index_aux(&[(a.addr, a.claim_hash)])),
        ("lease.d2", lease.encode()),
        ("receipt.accept.d2", rcpt.encode()),
    ];
    let per_item = rows(&a.ops, None);
    time_vector(
        "lease-online-grant-admits",
        a.rig,
        &refs(&a.ops),
        &aux,
        per_item,
    )
}

/// No lease held: (lease-missing, pending-dependency).
pub fn f9_lease_missing_pends() -> Vector {
    let a = deadline_arc("f9-lease-missing", true);
    let aux = [("index", index_aux(&[(a.addr, a.claim_hash)]))];
    let per_item = rows(&a.ops, Some(("lease-missing", "pending-dependency")));
    time_vector("lease-missing-pends", a.rig, &refs(&a.ops), &aux, per_item)
}

/// A valid lease whose only qualified receipt sits past
/// `expires + skew`: (lease-stale, quarantine-reproposal).
pub fn f9_lease_stale_quarantines() -> Vector {
    let mut a = deadline_arc("f9-lease-stale", true);
    let gid = grant_id_of(&a.ops[3].1);
    let (d2, lineage, addr) = (a.d2.clone(), a.d1.lineage, a.addr);
    let lease = a
        .rig
        .lease_stmt(&d2, gid, lineage, T0_MS, T0_MS + 86_400_000);
    // seen_ms = expires + skew + 100 s — conclusively outside T5's
    // window.
    let rcpt = a
        .rig
        .accept_receipt(&d2, addr, T0_MS + 86_400_000 + 300_000 + 100_000);
    let aux = [
        ("index", index_aux(&[(a.addr, a.claim_hash)])),
        ("lease.d2", lease.encode()),
        ("receipt.accept.d2", rcpt.encode()),
    ];
    let per_item = rows(&a.ops, Some(("lease-stale", "quarantine-reproposal")));
    time_vector(
        "lease-stale-quarantines",
        a.rig,
        &refs(&a.ops),
        &aux,
        per_item,
    )
}

/// The c.grant op's grant_id (the body's `grant.grant_id`).
fn grant_id_of(grant_op: &Signedop) -> [u8; 16] {
    let cbor::Value::Map(entries) = &grant_op.body else {
        panic!("grant body is a map")
    };
    let grant = entries
        .iter()
        .find_map(|(k, v)| (*k == cbor::Value::Text("grant".into())).then_some(v))
        .expect("grant field");
    let cbor::Value::Map(gfields) = grant else {
        panic!("grant is a map")
    };
    gfields
        .iter()
        .find_map(|(k, v)| {
            (*k == cbor::Value::Text("grant_id".into())).then(|| match v {
                cbor::Value::Bytes(b) => b.as_slice().try_into().expect("16 bytes"),
                _ => panic!("grant_id shape"),
            })
        })
        .expect("grant_id field")
}

/// A valid lease held but NO receipt at all: still awaiting
/// evidence — (lease-missing, pending-dependency), never stale.
pub fn f9_lease_present_no_receipt_pends() -> Vector {
    let mut a = deadline_arc("f9-lease-no-receipt", true);
    let gid = grant_id_of(&a.ops[3].1);
    let (d2, lineage) = (a.d2.clone(), a.d1.lineage);
    let lease = a
        .rig
        .lease_stmt(&d2, gid, lineage, T0_MS, T0_MS + 86_400_000);
    let aux = [
        ("index", index_aux(&[(a.addr, a.claim_hash)])),
        ("lease.d2", lease.encode()),
    ];
    let per_item = rows(&a.ops, Some(("lease-missing", "pending-dependency")));
    time_vector(
        "lease-present-no-receipt-pends",
        a.rig,
        &refs(&a.ops),
        &aux,
        per_item,
    )
}

/// A lease whose window exceeds `max_age_ms` is not a valid lease
/// (T5): with an otherwise in-window receipt held, the claim stays
/// (lease-missing, pending-dependency).
pub fn f9_lease_overlong_window_invalid() -> Vector {
    let mut a = deadline_arc("f9-lease-overlong", true);
    let gid = grant_id_of(&a.ops[3].1);
    let (d2, lineage, addr) = (a.d2.clone(), a.d1.lineage, a.addr);
    // Window = 3 days > max_age 2 days.
    let lease = a
        .rig
        .lease_stmt(&d2, gid, lineage, T0_MS, T0_MS + 3 * 86_400_000);
    let rcpt = a.rig.accept_receipt(&d2, addr, T0_MS + 43_200_000);
    let aux = [
        ("index", index_aux(&[(a.addr, a.claim_hash)])),
        ("lease.d2", lease.encode()),
        ("receipt.accept.d2", rcpt.encode()),
    ];
    let per_item = rows(&a.ops, Some(("lease-missing", "pending-dependency")));
    time_vector(
        "lease-overlong-window-invalid",
        a.rig,
        &refs(&a.ops),
        &aux,
        per_item,
    )
}

pub fn corpus_time() -> Vec<Vector> {
    vec![
        f9_deadline_receipted_admits(),
        f9_deadline_unreceipted_pends(),
        f9_deadline_self_receipt_nonqualifying(),
        f9_witnessless_zone_deadline_unusable(),
        f9_lease_online_grant_admits(),
        f9_lease_missing_pends(),
        f9_lease_stale_quarantines(),
        f9_lease_present_no_receipt_pends(),
        f9_lease_overlong_window_invalid(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_time_corpus_matches_builders() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vectors");
        for v in corpus_time() {
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
    fn time_corpus_checks_clean() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let companion: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("d0a-vector-cases.v1.json")).unwrap(),
        )
        .unwrap();
        for v in corpus_time() {
            crate::vector::check(&v.to_json(), &companion)
                .unwrap_or_else(|e| panic!("{}: {e}", v.name));
        }
    }
}
