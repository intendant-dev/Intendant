//! Appendix A.3 — control bodies (§7.1, tenant "ctrl").
//!
//! Each body struct carries `OP_TYPE` — the §7.1 registry's
//! `operation_type` string (envelope dispatch is
//! `(tenant, operation_type, operation_version)`; every control body
//! here is version 1). The requester-attestation payload builders
//! (`reauth`/`cutoffreq`/`abandonreq`) mint the exact signed message
//! maps (D-59 single-use, D-82/D-87 freshness).

use super::envelope::{gen_start, Actor, Header, Tenant, Writer};
use super::identity::{Authproof, Cert, Genesis, Grant};
use super::{
    bytes, key_bytes, sorted_set_by_key, sorted_set_default, text, u, Bytes16, Bytes32, Class,
    Erasemref, Fencecoord, Frontierclose, Head, Hlc, Issuerid, Kekwrap, Lineagedef, Polref,
    Ratifycutoff, Sigalg, Spaceclass, Spacedef, ToValue, Zoneheads, Zonepolicy,
};
use crate::cbor::{self, Value};

fn wrap_set(wraps: &[Kekwrap], what: &str) -> Value {
    Value::Array(sorted_set_by_key(
        wraps.iter().map(|w| (w.set_key(), w.to_value())).collect(),
        what,
    ))
}

fn frontierclose_set(closes: &[Frontierclose], what: &str) -> Value {
    Value::Array(sorted_set_by_key(
        closes
            .iter()
            .map(|c| {
                (
                    key_bytes(&[bytes(&c.zone_id), bytes(&c.lineage)]),
                    c.to_value(),
                )
            })
            .collect(),
        what,
    ))
}

/// `{ key_id: bytes32, through: uint, head_hash: bytes32 }` — the
/// per-issuer statement-feed boundary (D-87: head_hash = stmt_id of
/// statement #through, all-zero at 0). Used by `receipt_cutoffs` sets
/// (key `key_id`) and `cenrollrenew.feed_closure`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyFeedCutoff {
    pub key_id: Bytes32,
    pub through: u64,
    pub head_hash: Bytes32,
}

impl ToValue for KeyFeedCutoff {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("key_id", bytes(&self.key_id)),
            ("through", u(self.through)),
            ("head_hash", bytes(&self.head_hash)),
        ])
    }
}

fn receipt_cutoff_set(cuts: &[KeyFeedCutoff], what: &str) -> Value {
    Value::Array(sorted_set_by_key(
        cuts.iter()
            .map(|c| (key_bytes(&[bytes(&c.key_id)]), c.to_value()))
            .collect(),
        what,
    ))
}

/// `cservicekey = { service: "connect", alg: sigalg, pk: bstr,
///   ? receipt_cutoffs: [+ { key_id, through, head_hash }] }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cservicekey {
    pub alg: Sigalg,
    pub pk: Vec<u8>,
    pub receipt_cutoffs: Option<Vec<KeyFeedCutoff>>,
}

impl Cservicekey {
    pub const OP_TYPE: &'static str = "c.service_key";
}

impl ToValue for Cservicekey {
    fn to_value(&self) -> Value {
        let mut entries = vec![
            ("service", text("connect")),
            ("alg", self.alg.to_value()),
            ("pk", bytes(&self.pk)),
        ];
        if let Some(cs) = &self.receipt_cutoffs {
            entries.push((
                "receipt_cutoffs",
                receipt_cutoff_set(cs, "cservicekey.receipt_cutoffs"),
            ));
        }
        cbor::map(entries)
    }
}

/// `cgenesis = { descriptor: genesis, cert: cert, lineage: lineagedef,
///   zone: { zone_id: ulid, initial_epoch: 1, wraps: [+ kekwrap] },
///   home_space: spacedef, audit_space: spacedef,
///   zone_policy: zonepolicy, grant: grant, audit_grant: grant }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cgenesis {
    pub descriptor: Genesis,
    pub cert: Cert,
    pub lineage: Lineagedef,
    pub zone_id: Bytes16,
    pub zone_wraps: Vec<Kekwrap>,
    pub home_space: Spacedef,
    pub audit_space: Spacedef,
    pub zone_policy: Zonepolicy,
    pub grant: Grant,
    pub audit_grant: Grant,
}

impl Cgenesis {
    pub const OP_TYPE: &'static str = "c.genesis";
}

impl ToValue for Cgenesis {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("descriptor", self.descriptor.to_value()),
            ("cert", self.cert.to_value()),
            ("lineage", self.lineage.to_value()),
            (
                "zone",
                cbor::map(vec![
                    ("zone_id", bytes(&self.zone_id)),
                    ("initial_epoch", u(1)),
                    ("wraps", wrap_set(&self.zone_wraps, "cgenesis.zone.wraps")),
                ]),
            ),
            ("home_space", self.home_space.to_value()),
            ("audit_space", self.audit_space.to_value()),
            ("zone_policy", self.zone_policy.to_value()),
            ("grant", self.grant.to_value()),
            ("audit_grant", self.audit_grant.to_value()),
        ])
    }
}

/// `cenrollnew = { cert: cert, grants: [* grant], lineage: lineagedef,
///   wraps: [* kekwrap] }` — cert.renews ABSENT; grants set (E7, key
/// grant_id); wraps set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cenrollnew {
    pub cert: Cert,
    pub grants: Vec<Grant>,
    pub lineage: Lineagedef,
    pub wraps: Vec<Kekwrap>,
}

impl Cenrollnew {
    pub const OP_TYPE: &'static str = "c.enroll";
}

impl ToValue for Cenrollnew {
    fn to_value(&self) -> Value {
        let grants = sorted_set_by_key(
            self.grants
                .iter()
                .map(|g| (key_bytes(&[bytes(&g.grant_id)]), g.to_value()))
                .collect(),
            "cenrollnew.grants (key grant_id)",
        );
        cbor::map(vec![
            ("cert", self.cert.to_value()),
            ("grants", Value::Array(grants)),
            ("lineage", self.lineage.to_value()),
            ("wraps", wrap_set(&self.wraps, "cenrollnew.wraps")),
        ])
    }
}

/// `cenrollrenew = { cert: cert, feed_closure: { key_id, through,
///   head_hash }, history_cutoffs: [* frontierclose],
///   ? wraps: [* kekwrap] }` — cert.renews REQUIRED; feed_closure
/// closes the PREDECESSOR signing key's feed (D-111);
/// history_cutoffs = the supersede boundary over the predecessor's
/// authorship domain (D-141/D-143); wraps REQUIRED iff the KEM key
/// rotates (D-89/D-104).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cenrollrenew {
    pub cert: Cert,
    pub feed_closure: KeyFeedCutoff,
    pub history_cutoffs: Vec<Frontierclose>,
    pub wraps: Option<Vec<Kekwrap>>,
}

impl Cenrollrenew {
    pub const OP_TYPE: &'static str = "c.enroll";
}

impl ToValue for Cenrollrenew {
    fn to_value(&self) -> Value {
        let mut entries = vec![
            ("cert", self.cert.to_value()),
            ("feed_closure", self.feed_closure.to_value()),
            (
                "history_cutoffs",
                frontierclose_set(&self.history_cutoffs, "cenrollrenew.history_cutoffs"),
            ),
        ];
        if let Some(ws) = &self.wraps {
            entries.push(("wraps", wrap_set(ws, "cenrollrenew.wraps")));
        }
        cbor::map(entries)
    }
}

/// `crevokedev = { mode: "exclude" / "compromise", revocation_id,
///   cutoffs: [* frontierclose], ? receipt_cutoffs: [+ {...}],
///   rotation_refs: [* bytes32] }` — cutoffs cover the target's
/// AUTHORSHIP domain (may be empty, D-165); receipt_cutoffs REQUIRED
/// iff mode = "compromise"; rotation_refs = TYPED LINKAGE, never
/// coverage (D-180/D-195: completion is state-derived).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crevokedev {
    pub mode: RevokeMode,
    pub revocation_id: Bytes16,
    pub cutoffs: Vec<Frontierclose>,
    pub receipt_cutoffs: Option<Vec<KeyFeedCutoff>>,
    pub rotation_refs: Vec<Bytes32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeMode {
    Exclude,
    Compromise,
}

impl RevokeMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RevokeMode::Exclude => "exclude",
            RevokeMode::Compromise => "compromise",
        }
    }
}

impl Crevokedev {
    pub const OP_TYPE: &'static str = "c.revoke_device";
}

impl ToValue for Crevokedev {
    fn to_value(&self) -> Value {
        let mut entries = vec![
            ("mode", text(self.mode.as_str())),
            ("revocation_id", bytes(&self.revocation_id)),
            (
                "cutoffs",
                frontierclose_set(&self.cutoffs, "crevokedev.cutoffs"),
            ),
        ];
        if let Some(rc) = &self.receipt_cutoffs {
            entries.push((
                "receipt_cutoffs",
                receipt_cutoff_set(rc, "crevokedev.receipt_cutoffs"),
            ));
        }
        entries.push((
            "rotation_refs",
            Value::Array(sorted_set_default(
                self.rotation_refs.iter().map(|r| bytes(r)).collect(),
                "crevokedev.rotation_refs",
            )),
        ));
        cbor::map(entries)
    }
}

/// `crevokezones = { revocation_id: bytes16, ? rotation_refs: [+ bytes32],
///   ? cutoffs: [+ frontierclose] }` — at least one optional present
/// (continuation of the same revocation, D-71/D-186).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crevokezones {
    pub revocation_id: Bytes16,
    pub rotation_refs: Option<Vec<Bytes32>>,
    pub cutoffs: Option<Vec<Frontierclose>>,
}

impl Crevokezones {
    pub const OP_TYPE: &'static str = "c.revoke_zones";
}

impl ToValue for Crevokezones {
    fn to_value(&self) -> Value {
        let mut entries = vec![("revocation_id", bytes(&self.revocation_id))];
        if let Some(rs) = &self.rotation_refs {
            entries.push((
                "rotation_refs",
                Value::Array(sorted_set_default(
                    rs.iter().map(|r| bytes(r)).collect(),
                    "crevokezones.rotation_refs",
                )),
            ));
        }
        if let Some(cs) = &self.cutoffs {
            entries.push(("cutoffs", frontierclose_set(cs, "crevokezones.cutoffs")));
        }
        cbor::map(entries)
    }
}

/// `cwrapadd = { zone_id: ulid, epoch: uint, wrap: kekwrap }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cwrapadd {
    pub zone_id: Bytes16,
    pub epoch: u64,
    pub wrap: Kekwrap,
}

impl Cwrapadd {
    pub const OP_TYPE: &'static str = "c.wrap_add";
}

impl ToValue for Cwrapadd {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("zone_id", bytes(&self.zone_id)),
            ("epoch", u(self.epoch)),
            ("wrap", self.wrap.to_value()),
        ])
    }
}

/// `czonecreate = { zone_id: ulid, initial_epoch: 1,
///   wraps: [+ kekwrap], zone_policy: zonepolicy }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Czonecreate {
    pub zone_id: Bytes16,
    pub wraps: Vec<Kekwrap>,
    pub zone_policy: Zonepolicy,
}

impl Czonecreate {
    pub const OP_TYPE: &'static str = "c.zone_create";
}

impl ToValue for Czonecreate {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("zone_id", bytes(&self.zone_id)),
            ("initial_epoch", u(1)),
            ("wraps", wrap_set(&self.wraps, "czonecreate.wraps")),
            ("zone_policy", self.zone_policy.to_value()),
        ])
    }
}

/// `cspacecreate = spacedef`
pub type Cspacecreate = Spacedef;

/// The §7.1 operation_type for `cspacecreate` (the body is a bare
/// `spacedef`, so the constant cannot live on the aliased struct).
pub const CSPACECREATE_OP_TYPE: &str = "c.space_create";

/// `cspacepolicy = { space_id: ulid, ? space_class: spaceclass,
///   ? class_minimum: class, ? status_policy: polref }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cspacepolicy {
    pub space_id: Bytes16,
    pub space_class: Option<Spaceclass>,
    pub class_minimum: Option<Class>,
    pub status_policy: Option<Polref>,
}

impl Cspacepolicy {
    pub const OP_TYPE: &'static str = "c.space_policy_set";
}

impl ToValue for Cspacepolicy {
    fn to_value(&self) -> Value {
        let mut entries = vec![("space_id", bytes(&self.space_id))];
        if let Some(sc) = self.space_class {
            entries.push(("space_class", sc.to_value()));
        }
        if let Some(cm) = self.class_minimum {
            entries.push(("class_minimum", cm.to_value()));
        }
        if let Some(sp) = &self.status_policy {
            entries.push(("status_policy", sp.to_value()));
        }
        cbor::map(entries)
    }
}

/// `cspaceretire = { space_id: ulid, ? closes: [* frontierclose] }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cspaceretire {
    pub space_id: Bytes16,
    pub closes: Option<Vec<Frontierclose>>,
}

impl Cspaceretire {
    pub const OP_TYPE: &'static str = "c.space_retire";
}

impl ToValue for Cspaceretire {
    fn to_value(&self) -> Value {
        let mut entries = vec![("space_id", bytes(&self.space_id))];
        if let Some(cs) = &self.closes {
            entries.push(("closes", frontierclose_set(cs, "cspaceretire.closes")));
        }
        cbor::map(entries)
    }
}

/// `czonepolicy = { policy: zonepolicy, ? cutoffs: [* frontierclose] }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Czonepolicy {
    pub policy: Zonepolicy,
    pub cutoffs: Option<Vec<Frontierclose>>,
}

impl Czonepolicy {
    pub const OP_TYPE: &'static str = "c.zone_policy";
}

impl ToValue for Czonepolicy {
    fn to_value(&self) -> Value {
        let mut entries = vec![("policy", self.policy.to_value())];
        if let Some(cs) = &self.cutoffs {
            entries.push(("cutoffs", frontierclose_set(cs, "czonepolicy.cutoffs")));
        }
        cbor::map(entries)
    }
}

/// `cgrant = { grant: grant }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cgrant {
    pub grant: Grant,
}

impl Cgrant {
    pub const OP_TYPE: &'static str = "c.grant";
}

impl ToValue for Cgrant {
    fn to_value(&self) -> Value {
        cbor::map(vec![("grant", self.grant.to_value())])
    }
}

/// `crevokegrant = { grant_id: bytes16, ? cutoff: frontierclose }`
/// — cutoff REQUIRED when the grant is op-authoring (D-78/D-143).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crevokegrant {
    pub grant_id: Bytes16,
    pub cutoff: Option<Frontierclose>,
}

impl Crevokegrant {
    pub const OP_TYPE: &'static str = "c.revoke_grant";
}

impl ToValue for Crevokegrant {
    fn to_value(&self) -> Value {
        let mut entries = vec![("grant_id", bytes(&self.grant_id))];
        if let Some(c) = &self.cutoff {
            entries.push(("cutoff", c.to_value()));
        }
        cbor::map(entries)
    }
}

/// `cepochbump = { zone_id: ulid, new_epoch: uint,
///   ? cutoffs: [* frontierclose] }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cepochbump {
    pub zone_id: Bytes16,
    pub new_epoch: u64,
    pub cutoffs: Option<Vec<Frontierclose>>,
}

impl Cepochbump {
    pub const OP_TYPE: &'static str = "c.cap_epoch_bump";
}

impl ToValue for Cepochbump {
    fn to_value(&self) -> Value {
        let mut entries = vec![
            ("zone_id", bytes(&self.zone_id)),
            ("new_epoch", u(self.new_epoch)),
        ];
        if let Some(cs) = &self.cutoffs {
            entries.push(("cutoffs", frontierclose_set(cs, "cepochbump.cutoffs")));
        }
        cbor::map(entries)
    }
}

/// The D-59/D-82/D-87 freshness bundle every requester attestation
/// signs: single-use request_id + the signer's control view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReqFreshness {
    pub request_id: Bytes16,
    pub ctrl_frontier: Bytes32,
    pub lineage_version: u64,
    pub repoch: u64,
}

/// `requester: { device_cert: bytes32, ctrl_frontier: bytes32, sig: bstr }`
/// (clineagereauth / cabandon).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigRequester {
    pub device_cert: Bytes32,
    pub ctrl_frontier: Bytes32,
    pub sig: Vec<u8>,
}

impl ToValue for SigRequester {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("device_cert", bytes(&self.device_cert)),
            ("ctrl_frontier", bytes(&self.ctrl_frontier)),
            ("sig", bytes(&self.sig)),
        ])
    }
}

/// `clineagereauth = { lineage: bytes16, max_generations: uint,
///   requester: { device_cert, ctrl_frontier, sig } }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clineagereauth {
    pub lineage: Bytes16,
    pub max_generations: u64,
    pub requester: SigRequester,
}

impl Clineagereauth {
    pub const OP_TYPE: &'static str = "c.lineage_reauth";
}

impl ToValue for Clineagereauth {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("lineage", bytes(&self.lineage)),
            ("max_generations", u(self.max_generations)),
            ("requester", self.requester.to_value()),
        ])
    }
}

/// The signed reauth payload: `msg("reauth", { plane_id, lineage,
/// max_generations, request_id, ctrl_frontier, lineage_version,
/// repoch })` — this returns the canonical MAP bytes (the `x` the
/// signer frames under `Tag::Reauth`).
pub fn reauth_payload(
    plane_id: &Bytes32,
    lineage: &Bytes16,
    max_generations: u64,
    fresh: &ReqFreshness,
) -> Vec<u8> {
    let m = cbor::map(vec![
        ("plane_id", bytes(plane_id)),
        ("lineage", bytes(lineage)),
        ("max_generations", u(max_generations)),
        ("request_id", bytes(&fresh.request_id)),
        ("ctrl_frontier", bytes(&fresh.ctrl_frontier)),
        ("lineage_version", u(fresh.lineage_version)),
        ("repoch", u(fresh.repoch)),
    ]);
    cbor::encode(&m).expect("reauth payload encodes")
}

/// `ckekrotate = { zone_id: ulid, new_epoch: uint, wraps: [+ kekwrap],
///   erase_manifest: [* erasemref] }` — erase_manifest set keyed
/// item_addr (duplicates across rotations = idempotent skip, D-66).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ckekrotate {
    pub zone_id: Bytes16,
    pub new_epoch: u64,
    pub wraps: Vec<Kekwrap>,
    pub erase_manifest: Vec<Erasemref>,
}

impl Ckekrotate {
    pub const OP_TYPE: &'static str = "c.kek_rotate";
}

impl ToValue for Ckekrotate {
    fn to_value(&self) -> Value {
        let manifest = sorted_set_by_key(
            self.erase_manifest
                .iter()
                .map(|e| (key_bytes(&[bytes(&e.item_addr)]), e.to_value()))
                .collect(),
            "ckekrotate.erase_manifest (key item_addr)",
        );
        cbor::map(vec![
            ("zone_id", bytes(&self.zone_id)),
            ("new_epoch", u(self.new_epoch)),
            ("wraps", wrap_set(&self.wraps, "ckekrotate.wraps")),
            ("erase_manifest", Value::Array(manifest)),
        ])
    }
}

/// `requester: { device_cert, ctrl_frontier, live_heads: [* zoneheads],
///   sig }` (ccutoff) — live_heads: set keyed zone_id, CARRIED so the
/// signed message reconstructs from the body alone (D-95/D-100);
/// snapshot-wins (D-108).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutoffRequester {
    pub device_cert: Bytes32,
    pub ctrl_frontier: Bytes32,
    pub live_heads: Vec<Zoneheads>,
    pub sig: Vec<u8>,
}

fn live_heads_set(heads: &[Zoneheads], what: &str) -> Value {
    Value::Array(sorted_set_by_key(
        heads
            .iter()
            .map(|z| (key_bytes(&[bytes(&z.zone_id)]), z.to_value()))
            .collect(),
        what,
    ))
}

impl ToValue for CutoffRequester {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("device_cert", bytes(&self.device_cert)),
            ("ctrl_frontier", bytes(&self.ctrl_frontier)),
            (
                "live_heads",
                live_heads_set(&self.live_heads, "ccutoff.requester.live_heads"),
            ),
            ("sig", bytes(&self.sig)),
        ])
    }
}

/// `ccutoff = { cutoffs: [* ratifycutoff], ? closes: [* frontierclose],
///   ? requester: {...} }` — cutoffs set keyed (zone_id, lineage, gen);
/// closes = staged frontier closures (inert until consumed,
/// D-136/D-153/D-176).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ccutoff {
    pub cutoffs: Vec<Ratifycutoff>,
    pub closes: Option<Vec<Frontierclose>>,
    pub requester: Option<CutoffRequester>,
}

impl Ccutoff {
    pub const OP_TYPE: &'static str = "c.cutoff";
}

fn ratifycutoff_set(cuts: &[Ratifycutoff], what: &str) -> Vec<Value> {
    sorted_set_by_key(
        cuts.iter()
            .map(|c| {
                (
                    key_bytes(&[bytes(&c.zone_id), bytes(&c.lineage), u(c.gen)]),
                    c.to_value(),
                )
            })
            .collect(),
        what,
    )
}

impl ToValue for Ccutoff {
    fn to_value(&self) -> Value {
        let mut entries = vec![(
            "cutoffs",
            Value::Array(ratifycutoff_set(&self.cutoffs, "ccutoff.cutoffs")),
        )];
        if let Some(cs) = &self.closes {
            entries.push(("closes", frontierclose_set(cs, "ccutoff.closes")));
        }
        if let Some(r) = &self.requester {
            entries.push(("requester", r.to_value()));
        }
        cbor::map(entries)
    }
}

/// The signed cutoff-request payload: `msg("cutoffreq", { plane_id,
/// cutoffs, live_heads, request_id, ctrl_frontier, lineage_version,
/// repoch })` — canonical map bytes for `Tag::Cutoffreq`.
pub fn cutoffreq_payload(
    plane_id: &Bytes32,
    cutoffs: &[Ratifycutoff],
    live_heads: &[Zoneheads],
    fresh: &ReqFreshness,
) -> Vec<u8> {
    let m = cbor::map(vec![
        ("plane_id", bytes(plane_id)),
        (
            "cutoffs",
            Value::Array(ratifycutoff_set(cutoffs, "cutoffreq.cutoffs")),
        ),
        (
            "live_heads",
            live_heads_set(live_heads, "cutoffreq.live_heads"),
        ),
        ("request_id", bytes(&fresh.request_id)),
        ("ctrl_frontier", bytes(&fresh.ctrl_frontier)),
        ("lineage_version", u(fresh.lineage_version)),
        ("repoch", u(fresh.repoch)),
    ]);
    cbor::encode(&m).expect("cutoffreq payload encodes")
}

/// One `cabandon.seals` entry: `{ gen: uint, at: head / "none" }` —
/// `at = "none"` voids the generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seal {
    pub gen: u64,
    pub at: SealAt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealAt {
    At(Head),
    None,
}

impl ToValue for Seal {
    fn to_value(&self) -> Value {
        let at = match &self.at {
            SealAt::At(h) => h.to_value(),
            SealAt::None => text("none"),
        };
        cbor::map(vec![("gen", u(self.gen)), ("at", at)])
    }
}

fn seal_set(seals: &[Seal], what: &str) -> Vec<Value> {
    sorted_set_by_key(
        seals
            .iter()
            .map(|s| (key_bytes(&[u(s.gen)]), s.to_value()))
            .collect(),
        what,
    )
}

/// `cabandon = { zone_id: ulid, lineage: bytes16,
///   seals: [+ { gen: uint, at: head / "none" }], ? requester: {...} }`
/// — seals: non-empty set keyed gen (D-101/D-107/D-114).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cabandon {
    pub zone_id: Bytes16,
    pub lineage: Bytes16,
    pub seals: Vec<Seal>,
    pub requester: Option<SigRequester>,
}

impl Cabandon {
    pub const OP_TYPE: &'static str = "c.abandon_writer";
}

impl ToValue for Cabandon {
    fn to_value(&self) -> Value {
        assert!(
            !self.seals.is_empty(),
            "cabandon.seals is non-empty (D-114)"
        );
        let mut entries = vec![
            ("zone_id", bytes(&self.zone_id)),
            ("lineage", bytes(&self.lineage)),
            (
                "seals",
                Value::Array(seal_set(&self.seals, "cabandon.seals (key gen)")),
            ),
        ];
        if let Some(r) = &self.requester {
            entries.push(("requester", r.to_value()));
        }
        cbor::map(entries)
    }
}

/// The signed abandon-request payload: `msg("abandonreq", { plane_id,
/// zone_id, lineage, seals, request_id, ctrl_frontier,
/// lineage_version, repoch })` — canonical map bytes for
/// `Tag::Abandonreq`.
pub fn abandonreq_payload(
    plane_id: &Bytes32,
    zone_id: &Bytes16,
    lineage: &Bytes16,
    seals: &[Seal],
    fresh: &ReqFreshness,
) -> Vec<u8> {
    let m = cbor::map(vec![
        ("plane_id", bytes(plane_id)),
        ("zone_id", bytes(zone_id)),
        ("lineage", bytes(lineage)),
        (
            "seals",
            Value::Array(seal_set(seals, "abandonreq.seals (key gen)")),
        ),
        ("request_id", bytes(&fresh.request_id)),
        ("ctrl_frontier", bytes(&fresh.ctrl_frontier)),
        ("lineage_version", u(fresh.lineage_version)),
        ("repoch", u(fresh.repoch)),
    ]);
    cbor::encode(&m).expect("abandonreq payload encodes")
}

/// `checkpointobj = { zone_id: ulid, prev_checkpoint: bytes32 / "genesis",
///   covers: [* head], fences: [* fencecoord], retired: [* head],
///   proof_positions: [* { issuer: issuerid, through: uint,
///   head_hash: bytes32 }] }` — NO `v` (the envelope versions the
/// body, E6/D-88); covers/retired keyed (lineage, gen); fences keyed
/// lineage; proof_positions keyed by the tagged issuer.
/// `ccheckpoint = checkpointobj` — the body IS the object (D-80).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpointobj {
    pub zone_id: Bytes16,
    pub prev_checkpoint: PrevCheckpoint,
    pub covers: Vec<Head>,
    pub fences: Vec<Fencecoord>,
    pub retired: Vec<Head>,
    pub proof_positions: Vec<ProofPosition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrevCheckpoint {
    Op(Bytes32),
    Genesis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProofPosition {
    pub issuer: Issuerid,
    pub through: u64,
    pub head_hash: Bytes32,
}

impl ToValue for ProofPosition {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("issuer", self.issuer.to_value()),
            ("through", u(self.through)),
            ("head_hash", bytes(&self.head_hash)),
        ])
    }
}

impl Checkpointobj {
    pub const OP_TYPE: &'static str = "c.checkpoint";
}

fn head_set_by_lineage_gen(heads: &[Head], what: &str) -> Vec<Value> {
    sorted_set_by_key(
        heads
            .iter()
            .map(|h| (key_bytes(&[bytes(&h.lineage), u(h.gen)]), h.to_value()))
            .collect(),
        what,
    )
}

impl ToValue for Checkpointobj {
    fn to_value(&self) -> Value {
        let prev = match &self.prev_checkpoint {
            PrevCheckpoint::Op(h) => bytes(h),
            PrevCheckpoint::Genesis => text("genesis"),
        };
        let fences = sorted_set_by_key(
            self.fences
                .iter()
                .map(|f| (key_bytes(&[bytes(&f.lineage)]), f.to_value()))
                .collect(),
            "checkpointobj.fences (key lineage)",
        );
        let proofs = sorted_set_by_key(
            self.proof_positions
                .iter()
                .map(|p| {
                    (
                        cbor::encode(&p.issuer.to_value()).expect("issuer encodes"),
                        p.to_value(),
                    )
                })
                .collect(),
            "checkpointobj.proof_positions (tagged issuer)",
        );
        cbor::map(vec![
            ("zone_id", bytes(&self.zone_id)),
            ("prev_checkpoint", prev),
            (
                "covers",
                Value::Array(head_set_by_lineage_gen(
                    &self.covers,
                    "checkpointobj.covers",
                )),
            ),
            ("fences", Value::Array(fences)),
            (
                "retired",
                Value::Array(head_set_by_lineage_gen(
                    &self.retired,
                    "checkpointobj.retired",
                )),
            ),
            ("proof_positions", Value::Array(proofs)),
        ])
    }
}

/// `new_admin: { alg: sigalg, pk: bstr }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminKey {
    pub alg: Sigalg,
    pub pk: Vec<u8>,
}

impl ToValue for AdminKey {
    fn to_value(&self) -> Value {
        cbor::map(vec![("alg", self.alg.to_value()), ("pk", bytes(&self.pk))])
    }
}

/// `cadminsucc = { new_admin: { alg: sigalg, pk: bstr }, epoch: uint }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cadminsucc {
    pub new_admin: AdminKey,
    pub epoch: u64,
}

impl Cadminsucc {
    pub const OP_TYPE: &'static str = "c.admin_succession";
}

impl ToValue for Cadminsucc {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("new_admin", self.new_admin.to_value()),
            ("epoch", u(self.epoch)),
        ])
    }
}

/// `crecovsucc = { base: { seq: uint, op: bytes32 }, epoch: uint,
///   repoch: uint, new_admin: { alg, pk }, new_recovery_commitment,
///   tenant_cutoffs: [* frontierclose],
///   ? adopted_renewals: [* { device_id, renewal_op }],
///   ? retired_keys: [* bytes32], adopted_rotations: [* {...}] }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crecovsucc {
    pub base_seq: u64,
    pub base_op: Bytes32,
    pub epoch: u64,
    pub repoch: u64,
    pub new_admin: AdminKey,
    pub new_recovery_commitment: Bytes32,
    pub tenant_cutoffs: Vec<Frontierclose>,
    pub adopted_renewals: Option<Vec<AdoptedRenewal>>,
    pub retired_keys: Option<Vec<Bytes32>>,
    pub adopted_rotations: Vec<AdoptedRotation>,
}

/// `{ device_id: bytes16, renewal_op: bytes32 }` — keyed
/// (device_id, renewal_op) (D-150/D-172).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdoptedRenewal {
    pub device_id: Bytes16,
    pub renewal_op: Bytes32,
}

impl ToValue for AdoptedRenewal {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("device_id", bytes(&self.device_id)),
            ("renewal_op", bytes(&self.renewal_op)),
        ])
    }
}

/// `{ zone_id: ulid, rotation_op: bytes32, fence_frontier: bytes32,
///   control_frontier: bytes32, recipients_hash: bytes32 }` — keyed
/// (zone_id, rotation_op) (D-104/D-112/D-117).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdoptedRotation {
    pub zone_id: Bytes16,
    pub rotation_op: Bytes32,
    pub fence_frontier: Bytes32,
    pub control_frontier: Bytes32,
    pub recipients_hash: Bytes32,
}

impl ToValue for AdoptedRotation {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("zone_id", bytes(&self.zone_id)),
            ("rotation_op", bytes(&self.rotation_op)),
            ("fence_frontier", bytes(&self.fence_frontier)),
            ("control_frontier", bytes(&self.control_frontier)),
            ("recipients_hash", bytes(&self.recipients_hash)),
        ])
    }
}

impl Crecovsucc {
    pub const OP_TYPE: &'static str = "c.recovery_succession";
}

impl ToValue for Crecovsucc {
    fn to_value(&self) -> Value {
        let mut entries = vec![
            (
                "base",
                cbor::map(vec![
                    ("seq", u(self.base_seq)),
                    ("op", bytes(&self.base_op)),
                ]),
            ),
            ("epoch", u(self.epoch)),
            ("repoch", u(self.repoch)),
            ("new_admin", self.new_admin.to_value()),
            (
                "new_recovery_commitment",
                bytes(&self.new_recovery_commitment),
            ),
            (
                "tenant_cutoffs",
                frontierclose_set(&self.tenant_cutoffs, "crecovsucc.tenant_cutoffs"),
            ),
        ];
        if let Some(ars) = &self.adopted_renewals {
            let set = sorted_set_by_key(
                ars.iter()
                    .map(|a| {
                        (
                            key_bytes(&[bytes(&a.device_id), bytes(&a.renewal_op)]),
                            a.to_value(),
                        )
                    })
                    .collect(),
                "crecovsucc.adopted_renewals (key device_id, renewal_op)",
            );
            entries.push(("adopted_renewals", Value::Array(set)));
        }
        if let Some(rks) = &self.retired_keys {
            entries.push((
                "retired_keys",
                Value::Array(sorted_set_default(
                    rks.iter().map(|k| bytes(k)).collect(),
                    "crecovsucc.retired_keys",
                )),
            ));
        }
        let rots = sorted_set_by_key(
            self.adopted_rotations
                .iter()
                .map(|r| {
                    (
                        key_bytes(&[bytes(&r.zone_id), bytes(&r.rotation_op)]),
                        r.to_value(),
                    )
                })
                .collect(),
            "crecovsucc.adopted_rotations (key zone_id, rotation_op)",
        );
        entries.push(("adopted_rotations", Value::Array(rots)));
        cbor::map(entries)
    }
}

/// `cdrill = { nonce: bytes16 }`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cdrill {
    pub nonce: Bytes16,
}

impl Cdrill {
    pub const OP_TYPE: &'static str = "c.drill";
}

impl ToValue for Cdrill {
    fn to_value(&self) -> Value {
        cbor::map(vec![("nonce", bytes(&self.nonce))])
    }
}

/// O7 helper: the control-op header skeleton — tenant "ctrl",
/// `authored_kek_epoch = 0`, `capability_epoch = 0`,
/// `actor = { kind: "human", id: "owner" }`, no `attested_by`;
/// generation openers take `previous_writer_hash = gen_start` (O6).
/// The caller fills identity, proof, sequencing, and op naming.
#[allow(clippy::too_many_arguments)]
pub fn ctrl_header(
    plane_id: Bytes32,
    zone_id: Bytes16,
    space_id: Bytes16,
    signer_alg: Sigalg,
    signer_key_id: Bytes32,
    writer: Writer,
    authorization_proof: Authproof,
    request_id: Bytes16,
    writer_sequence: u64,
    previous_writer_hash: Option<Bytes32>,
    created_hlc: Hlc,
    operation_type: &str,
) -> Header {
    Header {
        tenant: Tenant::Ctrl,
        plane_id,
        zone_id,
        space_id,
        authored_kek_epoch: 0,
        capability_epoch: 0,
        signer_alg,
        signer_key_id,
        previous_writer_hash: previous_writer_hash
            .unwrap_or_else(|| gen_start(&writer.lineage, writer.gen)),
        writer,
        actor: Actor::owner(),
        authorization_proof,
        request_id,
        writer_sequence,
        causal_references: vec![],
        created_hlc,
        operation_type: operation_type.to_string(),
        operation_version: 1,
        body_hash: [0; 32],
    }
}

#[cfg(test)]
pub(crate) const CDDL_PINS_CONTROL: &[&str] = &[
    r#"ctrlbody = cgenesis / cenroll / crevokedev / crevokezones / cwrapadd
         / czonecreate / cspacecreate / cspacepolicy / cspaceretire
         / czonepolicy / cservicekey / cgrant / crevokegrant
         / cepochbump / clineagereauth / ckekrotate / ccutoff
         / cabandon / ccheckpoint / cadminsucc / crecovsucc / cdrill"#,
    r#"cservicekey = { service: "connect", alg: sigalg, pk: bstr,
  ? receipt_cutoffs: [+ { key_id: bytes32, through: uint,
                          head_hash: bytes32 }] }"#,
    r#"cgenesis = { descriptor: genesis, cert: cert, lineage: lineagedef,
  zone: { zone_id: ulid, initial_epoch: 1,
          wraps: [+ kekwrap] },   ; wraps set (E7)
  home_space: spacedef,     ; exactly one; personal / private / workflow-v1
  audit_space: spacedef,    ; exactly one; audit / private / owner-v1
  zone_policy: zonepolicy,  ; the B.1 template instantiated
  grant: grant,             ; finite budget REQUIRED (budgets posture)
  audit_grant: grant }      ; ops = ["audit.write"], audit space"#,
    r#"cenrollnew = { cert: cert,                  ; cert.renews ABSENT
  grants: [* grant],                        ; set (E7)
  lineage: lineagedef,
  wraps: [* kekwrap] }                      ; set (E7)"#,
    r#"cenrollrenew = { cert: cert,                ; cert.renews REQUIRED
  feed_closure: { key_id: bytes32, through: uint,
                  head_hash: bytes32 },"#,
    r#"  history_cutoffs: [* frontierclose],"#,
    r#"  ? wraps: [* kekwrap] }"#,
    r#"crevokedev = { mode: "exclude" / "compromise",
  revocation_id: bytes16,"#,
    r#"  cutoffs: [* frontierclose],  ; set; one per zone of the target's"#,
    r#"  ? receipt_cutoffs: [+ { key_id: bytes32, through: uint,
                          head_hash: bytes32 }],"#,
    r#"  rotation_refs: [* bytes32] }   ; set; separately committed"#,
    r#"crevokezones = { revocation_id: bytes16,
  ? rotation_refs: [+ bytes32],  ; set (E7)"#,
    r#"  ? cutoffs: [+ frontierclose] } ; set (E7); at least one field"#,
    r#"cwrapadd = { zone_id: ulid, epoch: uint, wrap: kekwrap }"#,
    r#"czonecreate = { zone_id: ulid, initial_epoch: 1,
  wraps: [+ kekwrap],                        ; set (E7)
  zone_policy: zonepolicy }
cspacecreate = spacedef
cspacepolicy = { space_id: ulid, ? space_class: spaceclass,
  ? class_minimum: class, ? status_policy: polref }
cspaceretire = { space_id: ulid, ? closes: [* frontierclose] }"#,
    r#"czonepolicy = { policy: zonepolicy, ? cutoffs: [* frontierclose] }"#,
    r#"cgrant = { grant: grant }
crevokegrant = { grant_id: bytes16, ? cutoff: frontierclose }"#,
    r#"cepochbump = { zone_id: ulid, new_epoch: uint,
  ? cutoffs: [* frontierclose] }"#,
    r#"clineagereauth = { lineage: bytes16, max_generations: uint,
  requester: { device_cert: bytes32, ctrl_frontier: bytes32, sig: bstr } }"#,
    r#"ckekrotate = { zone_id: ulid, new_epoch: uint,
  wraps: [+ kekwrap],                        ; set (E7)
  erase_manifest: [* erasemref] }            ; set (E7), sorted by"#,
    r#"ccutoff = { cutoffs: [* ratifycutoff],
  ? closes: [* frontierclose],
  ? requester: { device_cert: bytes32, ctrl_frontier: bytes32,
                 live_heads: [* zoneheads], sig: bstr } }"#,
    r#"cabandon = { zone_id: ulid, lineage: bytes16,
  seals: [+ { gen: uint, at: head / "none" }],   ; non-empty (D-114)
  ? requester: { device_cert: bytes32, ctrl_frontier: bytes32,
                 sig: bstr } }"#,
    r#"ccheckpoint = checkpointobj"#,
    r#"checkpointobj = { zone_id: ulid,   ; NO v — the envelope's"#,
    r#"  prev_checkpoint: bytes32 / "genesis","#,
    r#"  covers: [* head],                  ; set (E7), key (lineage, gen);"#,
    r#"  fences: [* fencecoord],            ; set (E7), key lineage; <= 256;"#,
    r#"  retired: [* head],                 ; set (E7), key (lineage, gen);"#,
    r#"  proof_positions: [* { issuer: issuerid, through: uint,
                        head_hash: bytes32 }] }"#,
    r#"cadminsucc = { new_admin: { alg: sigalg, pk: bstr }, epoch: uint }
crecovsucc = { base: { seq: uint, op: bytes32 }, epoch: uint,
  repoch: uint, new_admin: { alg: sigalg, pk: bstr },
  new_recovery_commitment: bytes32,
  tenant_cutoffs: [* frontierclose],"#,
    r#"  ? adopted_renewals: [* { device_id: bytes16,
                           renewal_op: bytes32 }],"#,
    r#"  ? retired_keys: [* bytes32],"#,
    r#"  adopted_rotations: [* { zone_id: ulid, rotation_op: bytes32,
    fence_frontier: bytes32, control_frontier: bytes32,
    recipients_hash: bytes32 }] }"#,
    r#"cdrill = { nonce: bytes16 }"#,
    r#"  ; sig = Sign(device_sk, msg("reauth", { plane_id, lineage,
  ;   max_generations, request_id, ctrl_frontier, lineage_version,
  ;   repoch }))"#,
    r#"  ;   = Sign(device_sk, msg("cutoffreq", { plane_id, cutoffs,
  ;   live_heads, request_id, ctrl_frontier, lineage_version,
  ;   repoch }))"#,
    r#"  ;   sig = Sign(device_sk, msg("abandonreq", { plane_id, zone_id,
  ;   lineage, seals, request_id, ctrl_frontier, lineage_version,
  ;   repoch }))"#,
];

#[cfg(test)]
mod tests {
    use super::super::{assert_pins, map_keys, spec_text};
    use super::*;

    fn kw(epoch: u64, device: u8) -> Kekwrap {
        Kekwrap {
            plane_id: [1; 32],
            zone_id: [2; 16],
            epoch,
            recipient_device: [device; 16],
            recipient_kem_key: [3; 32],
            enc: [4; 65],
            ct: [5; 48],
        }
    }

    fn fc(zone: u8, lineage: u8) -> Frontierclose {
        Frontierclose {
            zone_id: [zone; 16],
            lineage: [lineage; 16],
            heads: vec![],
        }
    }

    #[test]
    fn cddl_pins_are_verbatim() {
        assert_pins(CDDL_PINS_CONTROL);
    }

    #[test]
    fn op_type_strings_exist_in_the_registry() {
        let spec = spec_text();
        for op in [
            Cgenesis::OP_TYPE,
            Cenrollnew::OP_TYPE,
            Cenrollrenew::OP_TYPE,
            Crevokedev::OP_TYPE,
            Crevokezones::OP_TYPE,
            Cwrapadd::OP_TYPE,
            Czonecreate::OP_TYPE,
            CSPACECREATE_OP_TYPE,
            Cspacepolicy::OP_TYPE,
            Cspaceretire::OP_TYPE,
            Czonepolicy::OP_TYPE,
            Cservicekey::OP_TYPE,
            Cgrant::OP_TYPE,
            Crevokegrant::OP_TYPE,
            Cepochbump::OP_TYPE,
            Clineagereauth::OP_TYPE,
            Ckekrotate::OP_TYPE,
            Ccutoff::OP_TYPE,
            Cabandon::OP_TYPE,
            Checkpointobj::OP_TYPE,
            Cadminsucc::OP_TYPE,
            Crecovsucc::OP_TYPE,
            Cdrill::OP_TYPE,
        ] {
            assert!(
                spec.contains(&format!("`{op}`")),
                "operation_type `{op}` not found in the spec registry"
            );
        }
    }

    #[test]
    fn wrap_sets_sort_by_zone_epoch_device() {
        let body = Ckekrotate {
            zone_id: [2; 16],
            new_epoch: 3,
            wraps: vec![kw(2, 9), kw(2, 1), kw(1, 5)],
            erase_manifest: vec![],
        };
        let Value::Map(entries) = body.to_value() else {
            panic!()
        };
        let wraps = &entries
            .iter()
            .find(|(k, _)| *k == Value::Text("wraps".into()))
            .unwrap()
            .1;
        let Value::Array(items) = wraps else { panic!() };
        let order: Vec<(u64, u8)> = items
            .iter()
            .map(|w| {
                let Value::Map(fields) = w else { panic!() };
                let epoch = fields
                    .iter()
                    .find_map(|(k, v)| {
                        (*k == Value::Text("epoch".into())).then(|| match v {
                            Value::Uint(e) => *e,
                            _ => panic!(),
                        })
                    })
                    .unwrap();
                let dev = fields
                    .iter()
                    .find_map(|(k, v)| {
                        (*k == Value::Text("recipient_device".into())).then(|| match v {
                            Value::Bytes(b) => b[0],
                            _ => panic!(),
                        })
                    })
                    .unwrap();
                (epoch, dev)
            })
            .collect();
        assert_eq!(order, [(1, 5), (2, 1), (2, 9)]);
    }

    #[test]
    fn genesis_body_shape() {
        use super::super::identity::{Budget, GrantTenant, Provenance, SpacesSel, ZoneSel};
        use super::super::{DeadlineFallback, Strictness, Verb};
        let grant = Grant {
            plane_id: [1; 32],
            grant_id: [2; 16],
            subject_device: [3; 16],
            lineage: Some([4; 16]),
            tenants: vec![GrantTenant::Memory],
            zone: ZoneSel::Zone([5; 16]),
            spaces: SpacesSel::Spaces(vec![[6; 16]]),
            ops: vec![Verb::Propose],
            kinds: None,
            class_ceiling: Class::Sensitive,
            can_declassify: None,
            can_raise: None,
            raise_quota: None,
            flows: None,
            budget: Some(Budget {
                max_ops: 1_000_000,
                max_bytes: 268_435_456,
            }),
            online_lease: false,
            max_age_ms: None,
            issued_admin_epoch: 1,
            capability_epoch: 1,
            expiry_deadline_ms: None,
        };
        let body = Cgenesis {
            descriptor: Genesis {
                root_sig_alg: Sigalg::Ed25519,
                root_sig_pk: vec![7; 32],
                recovery_commitment: [8; 32],
                provenance: Provenance::Trusted,
                created_ms: 1,
            },
            cert: Cert {
                plane_id: [1; 32],
                device_id: [3; 16],
                sig_alg: Sigalg::Ed25519,
                sig_pk: vec![9; 32],
                kem_pk: vec![10; 65],
                class: Devclass::Daemon,
                evidence_hash: [11; 32],
                evidence_media_type: None,
                issued_admin_epoch: 1,
                expiry_deadline_ms: None,
                revocation_id: [12; 16],
                renews: None,
            },
            lineage: Lineagedef {
                lineage: [4; 16],
                device_id: [3; 16],
                max_generations: 8,
            },
            zone_id: [5; 16],
            zone_wraps: vec![kw(1, 3)],
            home_space: space([6; 16], Spaceclass::Personal),
            audit_space: space([13; 16], Spaceclass::Audit),
            zone_policy: Zonepolicy {
                zone_id: [5; 16],
                strictness: Strictness::Strict,
                deadline_fallback: DeadlineFallback::Budgets,
                require_cert_deadlines: false,
                grant_epoch_slack: None,
                time_witnesses: None,
                connect_service_key: None,
            },
            grant: grant.clone(),
            audit_grant: grant,
        };
        let v = body.to_value();
        assert_eq!(
            map_keys(&v),
            [
                "descriptor",
                "cert",
                "lineage",
                "zone",
                "home_space",
                "audit_space",
                "zone_policy",
                "grant",
                "audit_grant"
            ]
        );
        let Value::Map(entries) = &v else { panic!() };
        let zone = &entries
            .iter()
            .find(|(k, _)| *k == Value::Text("zone".into()))
            .unwrap()
            .1;
        assert_eq!(map_keys(zone), ["zone_id", "initial_epoch", "wraps"]);
        use super::super::Devclass;
        fn space(id: super::super::Bytes16, class: Spaceclass) -> Spacedef {
            Spacedef {
                space_id: id,
                zone_id: [5; 16],
                name_hash: [0; 32],
                space_class: class,
                class_minimum: Class::Private,
                status_policy: Polref {
                    id: "workflow-v1".into(),
                    version: 1,
                    hash: [0; 32],
                },
            }
        }
    }

    #[test]
    fn optional_fields_across_bodies() {
        // crevokezones: both optionals absent is non-canonical per the
        // comment, but the BUILDER only shapes bytes — cross-field
        // validity is fold logic. Check emission both ways.
        let bare = Crevokezones {
            revocation_id: [1; 16],
            rotation_refs: None,
            cutoffs: None,
        };
        assert_eq!(map_keys(&bare.to_value()), ["revocation_id"]);
        let full = Crevokezones {
            revocation_id: [1; 16],
            rotation_refs: Some(vec![[9; 32], [2; 32]]),
            cutoffs: Some(vec![fc(1, 1)]),
        };
        assert_eq!(
            map_keys(&full.to_value()),
            ["revocation_id", "rotation_refs", "cutoffs"]
        );
        let ret = Cspaceretire {
            space_id: [1; 16],
            closes: None,
        };
        assert_eq!(map_keys(&ret.to_value()), ["space_id"]);
        let rg = Crevokegrant {
            grant_id: [1; 16],
            cutoff: Some(fc(1, 2)),
        };
        assert_eq!(map_keys(&rg.to_value()), ["grant_id", "cutoff"]);
    }

    #[test]
    fn frontierclose_sets_key_on_zone_lineage() {
        let body = Crevokedev {
            mode: RevokeMode::Exclude,
            revocation_id: [1; 16],
            cutoffs: vec![fc(2, 2), fc(1, 9), fc(2, 1)],
            receipt_cutoffs: None,
            rotation_refs: vec![],
        };
        let Value::Map(entries) = body.to_value() else {
            panic!()
        };
        let cuts = &entries
            .iter()
            .find(|(k, _)| *k == Value::Text("cutoffs".into()))
            .unwrap()
            .1;
        let Value::Array(items) = cuts else { panic!() };
        let order: Vec<(u8, u8)> = items
            .iter()
            .map(|c| {
                let Value::Map(fields) = c else { panic!() };
                let get = |name: &str| {
                    fields
                        .iter()
                        .find_map(|(k, v)| {
                            (*k == Value::Text(name.into())).then(|| match v {
                                Value::Bytes(b) => b[0],
                                _ => panic!(),
                            })
                        })
                        .unwrap()
                };
                (get("zone_id"), get("lineage"))
            })
            .collect();
        assert_eq!(order, [(1, 9), (2, 1), (2, 2)]);
    }

    #[test]
    fn checkpoint_shape_and_seal_arms() {
        let cp = Checkpointobj {
            zone_id: [1; 16],
            prev_checkpoint: PrevCheckpoint::Genesis,
            covers: vec![],
            fences: vec![],
            retired: vec![],
            proof_positions: vec![
                ProofPosition {
                    issuer: Issuerid::Service { key_id: [2; 32] },
                    through: 5,
                    head_hash: [3; 32],
                },
                ProofPosition {
                    issuer: Issuerid::Device { cert: [1; 32] },
                    through: 2,
                    head_hash: [4; 32],
                },
            ],
        };
        let v = cp.to_value();
        assert_eq!(
            map_keys(&v),
            [
                "zone_id",
                "prev_checkpoint",
                "covers",
                "fences",
                "retired",
                "proof_positions"
            ]
        );
        let Value::Map(entries) = &v else { panic!() };
        assert!(entries.contains(&(
            Value::Text("prev_checkpoint".into()),
            Value::Text("genesis".into())
        )));

        let seal_none = Seal {
            gen: 2,
            at: SealAt::None,
        };
        let Value::Map(fields) = seal_none.to_value() else {
            panic!()
        };
        assert!(fields.contains(&(Value::Text("at".into()), Value::Text("none".into()))));
        // Empty seals panic (non-empty, D-114).
        let bad = Cabandon {
            zone_id: [1; 16],
            lineage: [2; 16],
            seals: vec![],
            requester: None,
        };
        assert!(std::panic::catch_unwind(move || bad.to_value()).is_err());
    }

    #[test]
    fn requester_payloads_are_distinct_and_deterministic() {
        let fresh = ReqFreshness {
            request_id: [1; 16],
            ctrl_frontier: [2; 32],
            lineage_version: 0,
            repoch: 0,
        };
        let reauth = reauth_payload(&[3; 32], &[4; 16], 8, &fresh);
        let cutoff = cutoffreq_payload(&[3; 32], &[], &[], &fresh);
        let abandon = abandonreq_payload(
            &[3; 32],
            &[5; 16],
            &[4; 16],
            &[Seal {
                gen: 1,
                at: SealAt::None,
            }],
            &fresh,
        );
        assert_ne!(reauth, cutoff);
        assert_ne!(cutoff, abandon);
        assert_eq!(reauth, reauth_payload(&[3; 32], &[4; 16], 8, &fresh));
        // Payloads sign under their OWN tags — distinct domains even
        // if payload bytes collided.
        use crate::domains::{h_tag, Tag};
        assert_ne!(h_tag(Tag::Reauth, &reauth), h_tag(Tag::Cutoffreq, &reauth));
    }

    #[test]
    fn ctrl_header_o7_pins() {
        let h = ctrl_header(
            [1; 32],
            [2; 16],
            [3; 16],
            Sigalg::Ed25519,
            [4; 32],
            Writer {
                lineage: [5; 16],
                gen: 1,
            },
            Authproof::Admin {
                epoch: 1,
                ctrl_frontier: [6; 32],
            },
            [7; 16],
            1,
            None,
            Hlc { ms: 1, count: 0 },
            Cgrant::OP_TYPE,
        );
        assert_eq!(h.authored_kek_epoch, 0);
        assert_eq!(h.capability_epoch, 0);
        assert_eq!(h.actor, Actor::owner());
        assert_eq!(h.previous_writer_hash, gen_start(&[5; 16], 1));
        assert_eq!(h.operation_version, 1);
    }
}
