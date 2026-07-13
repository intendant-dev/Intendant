//! Appendix A.1 — genesis / cert / grant / proof (§4.1–4.4).

use super::{bytes, text, u, Bytes16, Bytes32, Class, Devclass, Kind, Sigalg, ToValue, Verb};
use crate::cbor::{self, Value};

/// `genesis = { v: 1, suite: "suite-v1", root_sig_alg: sigalg,
///   root_sig_pk: bstr, recovery_commitment: bytes32,
///   governance: { v: 1, kind: "single-owner" },
///   provenance: "trusted" / "hosted", created_ms: ms }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Genesis {
    pub root_sig_alg: Sigalg,
    pub root_sig_pk: Vec<u8>,
    pub recovery_commitment: Bytes32,
    pub provenance: Provenance,
    pub created_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    Trusted,
    Hosted,
}

impl Provenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Provenance::Trusted => "trusted",
            Provenance::Hosted => "hosted",
        }
    }
}

impl ToValue for Genesis {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("v", u(1)),
            ("suite", text("suite-v1")),
            ("root_sig_alg", self.root_sig_alg.to_value()),
            ("root_sig_pk", bytes(&self.root_sig_pk)),
            ("recovery_commitment", bytes(&self.recovery_commitment)),
            (
                "governance",
                cbor::map(vec![("v", u(1)), ("kind", text("single-owner"))]),
            ),
            ("provenance", text(self.provenance.as_str())),
            ("created_ms", u(self.created_ms)),
        ])
    }
}

/// `cert = { v: 1, plane_id: bytes32, device_id: bytes16,
///   sig_alg: sigalg, sig_pk: bstr, kem_alg: "hpke-p256-v1", kem_pk: bstr,
///   class: devclass, evidence_hash: bytes32, ? evidence_media_type: text,
///   issued_admin_epoch: uint, ? expiry_deadline_ms: ms,
///   revocation_id: bytes16, ? renews: bytes32 }`
/// — `renews = H_cert(predecessor certificate bytes)` (D-85).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cert {
    pub plane_id: Bytes32,
    pub device_id: Bytes16,
    pub sig_alg: Sigalg,
    pub sig_pk: Vec<u8>,
    pub kem_pk: Vec<u8>,
    pub class: Devclass,
    pub evidence_hash: Bytes32,
    pub evidence_media_type: Option<String>,
    pub issued_admin_epoch: u64,
    pub expiry_deadline_ms: Option<u64>,
    pub revocation_id: Bytes16,
    pub renews: Option<Bytes32>,
}

impl ToValue for Cert {
    fn to_value(&self) -> Value {
        let mut entries = vec![
            ("v", u(1)),
            ("plane_id", bytes(&self.plane_id)),
            ("device_id", bytes(&self.device_id)),
            ("sig_alg", self.sig_alg.to_value()),
            ("sig_pk", bytes(&self.sig_pk)),
            ("kem_alg", text("hpke-p256-v1")),
            ("kem_pk", bytes(&self.kem_pk)),
            ("class", self.class.to_value()),
            ("evidence_hash", bytes(&self.evidence_hash)),
        ];
        if let Some(m) = &self.evidence_media_type {
            entries.push(("evidence_media_type", text(m)));
        }
        entries.push(("issued_admin_epoch", u(self.issued_admin_epoch)));
        if let Some(e) = self.expiry_deadline_ms {
            entries.push(("expiry_deadline_ms", u(e)));
        }
        entries.push(("revocation_id", bytes(&self.revocation_id)));
        if let Some(r) = &self.renews {
            entries.push(("renews", bytes(r)));
        }
        cbor::map(entries)
    }
}

/// One `grant.tenants` entry: `"memory" / "agenda"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantTenant {
    Memory,
    Agenda,
}

impl GrantTenant {
    pub fn as_str(self) -> &'static str {
        match self {
            GrantTenant::Memory => "memory",
            GrantTenant::Agenda => "agenda",
        }
    }
}

/// `zone: ulid / "*"` — the wildcard is legal only for the read-only
/// verb subset with `capability_epoch = 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneSel {
    Zone(Bytes16),
    Wildcard,
}

impl ToValue for ZoneSel {
    fn to_value(&self) -> Value {
        match self {
            ZoneSel::Zone(z) => bytes(z),
            ZoneSel::Wildcard => text("*"),
        }
    }
}

/// `spaces: [+ ulid] / "*"`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpacesSel {
    Spaces(Vec<Bytes16>),
    Wildcard,
}

impl ToValue for SpacesSel {
    fn to_value(&self) -> Value {
        match self {
            SpacesSel::Spaces(ids) => Value::Array(ids.iter().map(|s| bytes(s)).collect()),
            SpacesSel::Wildcard => text("*"),
        }
    }
}

/// `? budget: { max_ops: uint, max_bytes: uint }`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub max_ops: u64,
    pub max_bytes: u64,
}

impl ToValue for Budget {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("max_ops", u(self.max_ops)),
            ("max_bytes", u(self.max_bytes)),
        ])
    }
}

/// `grant = { v: 1, plane_id: bytes32, grant_id: bytes16,
///   subject_device: bytes16, ? lineage: bytes16,
///   tenants: [+ ("memory" / "agenda")], zone: ulid / "*",
///   spaces: [+ ulid] / "*", ops: [+ verb], ? kinds: [+ kind],
///   class_ceiling: class, ? can_declassify: bool, ? can_raise: bool,
///   ? raise_quota: uint, ? flows: [+ flow],
///   ? budget: { max_ops: uint, max_bytes: uint },
///   online_lease: bool, ? max_age_ms: ms,
///   issued_admin_epoch: uint, capability_epoch: uint,
///   ? expiry_deadline_ms: ms }`
///
/// `tenants`/`spaces`/`ops`/`kinds`/`flows` are NOT documented as E7
/// sets — the builder preserves caller order (E9 makes the writer's
/// order canonical); cross-field rules pin exact contents where they
/// apply (the genesis grants pin `tenants = ["memory"]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub plane_id: Bytes32,
    pub grant_id: Bytes16,
    pub subject_device: Bytes16,
    pub lineage: Option<Bytes16>,
    pub tenants: Vec<GrantTenant>,
    pub zone: ZoneSel,
    pub spaces: SpacesSel,
    pub ops: Vec<Verb>,
    pub kinds: Option<Vec<Kind>>,
    pub class_ceiling: Class,
    pub can_declassify: Option<bool>,
    pub can_raise: Option<bool>,
    pub raise_quota: Option<u64>,
    pub flows: Option<Vec<Flow>>,
    pub budget: Option<Budget>,
    pub online_lease: bool,
    pub max_age_ms: Option<u64>,
    pub issued_admin_epoch: u64,
    pub capability_epoch: u64,
    pub expiry_deadline_ms: Option<u64>,
}

impl ToValue for Grant {
    fn to_value(&self) -> Value {
        let mut entries = vec![
            ("v", u(1)),
            ("plane_id", bytes(&self.plane_id)),
            ("grant_id", bytes(&self.grant_id)),
            ("subject_device", bytes(&self.subject_device)),
        ];
        if let Some(l) = &self.lineage {
            entries.push(("lineage", bytes(l)));
        }
        entries.push((
            "tenants",
            Value::Array(self.tenants.iter().map(|t| text(t.as_str())).collect()),
        ));
        entries.push(("zone", self.zone.to_value()));
        entries.push(("spaces", self.spaces.to_value()));
        entries.push((
            "ops",
            Value::Array(self.ops.iter().map(|o| o.to_value()).collect()),
        ));
        if let Some(ks) = &self.kinds {
            entries.push((
                "kinds",
                Value::Array(ks.iter().map(|k| k.to_value()).collect()),
            ));
        }
        entries.push(("class_ceiling", self.class_ceiling.to_value()));
        if let Some(b) = self.can_declassify {
            entries.push(("can_declassify", Value::Bool(b)));
        }
        if let Some(b) = self.can_raise {
            entries.push(("can_raise", Value::Bool(b)));
        }
        if let Some(q) = self.raise_quota {
            entries.push(("raise_quota", u(q)));
        }
        if let Some(fs) = &self.flows {
            entries.push((
                "flows",
                Value::Array(fs.iter().map(|f| f.to_value()).collect()),
            ));
        }
        if let Some(b) = &self.budget {
            entries.push(("budget", b.to_value()));
        }
        entries.push(("online_lease", Value::Bool(self.online_lease)));
        if let Some(m) = self.max_age_ms {
            entries.push(("max_age_ms", u(m)));
        }
        entries.push(("issued_admin_epoch", u(self.issued_admin_epoch)));
        entries.push(("capability_epoch", u(self.capability_epoch)));
        if let Some(e) = self.expiry_deadline_ms {
            entries.push(("expiry_deadline_ms", u(e)));
        }
        cbor::map(entries)
    }
}

/// `flow = { from_zone: ulid, ? from_space: ulid, to: endpoint,
///   ? kinds: [+ kind], class_ceiling: class, expiry_deadline_ms: ms }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flow {
    pub from_zone: Bytes16,
    pub from_space: Option<Bytes16>,
    pub to: Endpoint,
    pub kinds: Option<Vec<Kind>>,
    pub class_ceiling: Class,
    pub expiry_deadline_ms: u64,
}

impl ToValue for Flow {
    fn to_value(&self) -> Value {
        let mut entries = vec![("from_zone", bytes(&self.from_zone))];
        if let Some(s) = &self.from_space {
            entries.push(("from_space", bytes(s)));
        }
        entries.push(("to", self.to.to_value()));
        if let Some(ks) = &self.kinds {
            entries.push((
                "kinds",
                Value::Array(ks.iter().map(|k| k.to_value()).collect()),
            ));
        }
        entries.push(("class_ceiling", self.class_ceiling.to_value()));
        entries.push(("expiry_deadline_ms", u(self.expiry_deadline_ms)));
        cbor::map(entries)
    }
}

/// `endpoint = { plane_id: bytes32, zone_id: ulid, space_id: ulid }
///            / { egress: { kind: ..., provider_id: text, profile_hash: bytes32 } }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Plane {
        plane_id: Bytes32,
        zone_id: Bytes16,
        space_id: Bytes16,
    },
    Egress {
        kind: EgressKind,
        provider_id: String,
        profile_hash: Bytes32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressKind {
    ModelProvider,
    Embedding,
    Reflection,
}

impl EgressKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EgressKind::ModelProvider => "model-provider",
            EgressKind::Embedding => "embedding",
            EgressKind::Reflection => "reflection",
        }
    }
}

impl ToValue for Endpoint {
    fn to_value(&self) -> Value {
        match self {
            Endpoint::Plane {
                plane_id,
                zone_id,
                space_id,
            } => cbor::map(vec![
                ("plane_id", bytes(plane_id)),
                ("zone_id", bytes(zone_id)),
                ("space_id", bytes(space_id)),
            ]),
            Endpoint::Egress {
                kind,
                provider_id,
                profile_hash,
            } => cbor::map(vec![(
                "egress",
                cbor::map(vec![
                    ("kind", text(kind.as_str())),
                    ("provider_id", text(provider_id)),
                    ("profile_hash", bytes(profile_hash)),
                ]),
            )]),
        }
    }
}

/// `authproof = { arm: "dev", cert: bytes32, cap: bytes32 }
///            / { arm: "genesis", genesis: bytes32 }
///            / { arm: "admin", epoch: uint, ctrl_frontier: bytes32 }
///            / { arm: "recovery", repoch: uint, recovery_pk: bstr .size 32 }`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authproof {
    Dev { cert: Bytes32, cap: Bytes32 },
    Genesis { genesis: Bytes32 },
    Admin { epoch: u64, ctrl_frontier: Bytes32 },
    Recovery { repoch: u64, recovery_pk: Bytes32 },
}

impl ToValue for Authproof {
    fn to_value(&self) -> Value {
        match self {
            Authproof::Dev { cert, cap } => cbor::map(vec![
                ("arm", text("dev")),
                ("cert", bytes(cert)),
                ("cap", bytes(cap)),
            ]),
            Authproof::Genesis { genesis } => {
                cbor::map(vec![("arm", text("genesis")), ("genesis", bytes(genesis))])
            }
            Authproof::Admin {
                epoch,
                ctrl_frontier,
            } => cbor::map(vec![
                ("arm", text("admin")),
                ("epoch", u(*epoch)),
                ("ctrl_frontier", bytes(ctrl_frontier)),
            ]),
            Authproof::Recovery {
                repoch,
                recovery_pk,
            } => cbor::map(vec![
                ("arm", text("recovery")),
                ("repoch", u(*repoch)),
                ("recovery_pk", bytes(recovery_pk)),
            ]),
        }
    }
}

#[cfg(test)]
pub(crate) const CDDL_PINS_IDENTITY: &[&str] = &[
    r#"genesis = { v: 1, suite: "suite-v1", root_sig_alg: sigalg,
  root_sig_pk: bstr, recovery_commitment: bytes32,
  governance: { v: 1, kind: "single-owner" },
  provenance: "trusted" / "hosted", created_ms: ms }
cert = { v: 1, plane_id: bytes32, device_id: bytes16,
  sig_alg: sigalg, sig_pk: bstr, kem_alg: "hpke-p256-v1", kem_pk: bstr,
  class: devclass, evidence_hash: bytes32, ? evidence_media_type: text,
  issued_admin_epoch: uint, ? expiry_deadline_ms: ms,
  revocation_id: bytes16, ? renews: bytes32 }"#,
    r#"grant = { v: 1, plane_id: bytes32, grant_id: bytes16,
  subject_device: bytes16, ? lineage: bytes16,
  tenants: [+ ("memory" / "agenda")], zone: ulid / "*",
  spaces: [+ ulid] / "*", ops: [+ verb], ? kinds: [+ kind],
  class_ceiling: class, ? can_declassify: bool, ? can_raise: bool,
  ? raise_quota: uint, ? flows: [+ flow],
  ? budget: { max_ops: uint, max_bytes: uint },
  online_lease: bool, ? max_age_ms: ms,
  issued_admin_epoch: uint, capability_epoch: uint,
  ? expiry_deadline_ms: ms }"#,
    r#"flow = { from_zone: ulid, ? from_space: ulid, to: endpoint,
  ? kinds: [+ kind], class_ceiling: class, expiry_deadline_ms: ms }
endpoint = { plane_id: bytes32, zone_id: ulid, space_id: ulid }"#,
    r#"         / { egress: { kind: "model-provider" / "embedding" / "reflection",
                       provider_id: text, profile_hash: bytes32 } }
authproof = { arm: "dev", cert: bytes32, cap: bytes32 }"#,
    r#"          / { arm: "genesis", genesis: bytes32 }"#,
    r#"          / { arm: "admin", epoch: uint, ctrl_frontier: bytes32 }
          / { arm: "recovery", repoch: uint, recovery_pk: bstr .size 32 }"#,
];

#[cfg(test)]
mod tests {
    use super::super::{assert_pins, map_keys};
    use super::*;

    #[test]
    fn cddl_pins_are_verbatim() {
        assert_pins(CDDL_PINS_IDENTITY);
    }

    #[test]
    fn genesis_fixed_literals() {
        let g = Genesis {
            root_sig_alg: Sigalg::Ed25519,
            root_sig_pk: vec![1; 32],
            recovery_commitment: [2; 32],
            provenance: Provenance::Trusted,
            created_ms: 1000,
        };
        let v = g.to_value();
        assert_eq!(
            map_keys(&v),
            [
                "v",
                "suite",
                "root_sig_alg",
                "root_sig_pk",
                "recovery_commitment",
                "governance",
                "provenance",
                "created_ms"
            ]
        );
        let Value::Map(entries) = &v else { panic!() };
        assert!(entries.contains(&(Value::Text("suite".into()), Value::Text("suite-v1".into()))));
        let gov = &entries
            .iter()
            .find(|(k, _)| *k == Value::Text("governance".into()))
            .unwrap()
            .1;
        assert_eq!(map_keys(gov), ["v", "kind"]);
    }

    #[test]
    fn cert_optionals_omitted_and_ordered() {
        let base = Cert {
            plane_id: [1; 32],
            device_id: [2; 16],
            sig_alg: Sigalg::Ed25519,
            sig_pk: vec![3; 32],
            kem_pk: vec![4; 65],
            class: Devclass::Daemon,
            evidence_hash: [5; 32],
            evidence_media_type: None,
            issued_admin_epoch: 1,
            expiry_deadline_ms: None,
            revocation_id: [6; 16],
            renews: None,
        };
        assert_eq!(
            map_keys(&base.to_value()),
            [
                "v",
                "plane_id",
                "device_id",
                "sig_alg",
                "sig_pk",
                "kem_alg",
                "kem_pk",
                "class",
                "evidence_hash",
                "issued_admin_epoch",
                "revocation_id"
            ]
        );
        let full = Cert {
            evidence_media_type: Some("image/png".into()),
            expiry_deadline_ms: Some(9),
            renews: Some([7; 32]),
            ..base
        };
        assert_eq!(map_keys(&full.to_value()).len(), 14);
    }

    #[test]
    fn grant_fields_and_selectors() {
        let g = Grant {
            plane_id: [1; 32],
            grant_id: [2; 16],
            subject_device: [3; 16],
            lineage: Some([4; 16]),
            tenants: vec![GrantTenant::Memory],
            zone: ZoneSel::Zone([5; 16]),
            spaces: SpacesSel::Spaces(vec![[6; 16]]),
            ops: vec![Verb::Propose, Verb::Read],
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
        let v = g.to_value();
        assert_eq!(
            map_keys(&v),
            [
                "v",
                "plane_id",
                "grant_id",
                "subject_device",
                "lineage",
                "tenants",
                "zone",
                "spaces",
                "ops",
                "class_ceiling",
                "budget",
                "online_lease",
                "issued_admin_epoch",
                "capability_epoch"
            ]
        );
        // Wildcards encode as the text "*".
        assert_eq!(ZoneSel::Wildcard.to_value(), Value::Text("*".into()));
        assert_eq!(SpacesSel::Wildcard.to_value(), Value::Text("*".into()));
        // ops preserve caller order (not an E7 set).
        let Value::Map(entries) = &v else { panic!() };
        let ops = &entries
            .iter()
            .find(|(k, _)| *k == Value::Text("ops".into()))
            .unwrap()
            .1;
        assert_eq!(
            ops,
            &Value::Array(vec![
                Value::Text("propose".into()),
                Value::Text("read".into())
            ])
        );
    }

    #[test]
    fn endpoint_and_authproof_arms() {
        let p = Endpoint::Plane {
            plane_id: [1; 32],
            zone_id: [2; 16],
            space_id: [3; 16],
        };
        assert_eq!(map_keys(&p.to_value()), ["plane_id", "zone_id", "space_id"]);
        let e = Endpoint::Egress {
            kind: EgressKind::ModelProvider,
            provider_id: "anthropic".into(),
            profile_hash: [4; 32],
        };
        assert_eq!(map_keys(&e.to_value()), ["egress"]);

        assert_eq!(
            map_keys(
                &Authproof::Dev {
                    cert: [1; 32],
                    cap: [2; 32]
                }
                .to_value()
            ),
            ["arm", "cert", "cap"]
        );
        assert_eq!(
            map_keys(&Authproof::Genesis { genesis: [3; 32] }.to_value()),
            ["arm", "genesis"]
        );
        assert_eq!(
            map_keys(
                &Authproof::Admin {
                    epoch: 1,
                    ctrl_frontier: [4; 32]
                }
                .to_value()
            ),
            ["arm", "epoch", "ctrl_frontier"]
        );
        assert_eq!(
            map_keys(
                &Authproof::Recovery {
                    repoch: 0,
                    recovery_pk: [5; 32]
                }
                .to_value()
            ),
            ["arm", "repoch", "recovery_pk"]
        );
    }

    #[test]
    fn flow_optionals() {
        let f = Flow {
            from_zone: [1; 16],
            from_space: None,
            to: Endpoint::Plane {
                plane_id: [2; 32],
                zone_id: [3; 16],
                space_id: [4; 16],
            },
            kinds: Some(vec![Kind::Observation]),
            class_ceiling: Class::Internal,
            expiry_deadline_ms: 99,
        };
        assert_eq!(
            map_keys(&f.to_value()),
            [
                "from_zone",
                "to",
                "kinds",
                "class_ceiling",
                "expiry_deadline_ms"
            ]
        );
    }
}
