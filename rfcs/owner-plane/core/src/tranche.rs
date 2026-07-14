//! The red-fixture opening tranche — the companion schema's
//! `x-informative.opening-tranche` annex, minted as real bytes.
//!
//! Every operation here is a genuine signed triple: real keys (drawn
//! from the vector's named ChaCha20 stream), real HPKE wraps, real
//! chain arithmetic — a reducer cannot admit these fixtures without
//! actually verifying them. The EXPECTED classifications are authored
//! from the normative text (each fixture cites its decision); the
//! reference reducer and the independent reducer must both reproduce
//! them — that is what makes the tranche red until a reducer exists.
//!
//! Committed artifacts: `../vectors/f{family:02}-{name}.json`
//! (written by `cargo run --bin mint`; the drift-gate test pins the
//! committed bytes to the builders).

use serde_json::{json, Map as JsonMap, Value as Json};

use crate::cbor;
use crate::domains::{h_tag, Tag};
use crate::keyschedule;
use crate::scenario;
use crate::shapes::control::{
    ctrl_header, Cenrollnew, Cgenesis, Cgrant, Ckekrotate, Crevokedev, RevokeMode,
};
use crate::shapes::envelope::{
    gen_start, seal_op, Actor, ActorKind, Header, OpSigner, Signedop, Tenant, Writer,
};
use crate::shapes::identity::{
    Authproof, Budget, Cert, Genesis, Grant, GrantTenant, Provenance, SpacesSel, ZoneSel,
};
use crate::shapes::memory::Mclaim;
use crate::shapes::{
    Bytes16, Bytes32, Class, Devclass, Frontierclose, Hlc, Kekwrap, Kind, Lineagedef, Polref,
    Sigalg, Spaceclass, Spacedef, ToValue, Verb,
};
use crate::suite::{self, hpke_wrap};
use crate::vector::{hex, Expected, RecordingRng, Vector};

/// N1 reserved identifiers (real IDs never have their first 8 bytes
/// all zero — the rig asserts that on every drawn ID).
pub const CTRL_ZONE: Bytes16 = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01];
pub const CTRL_SPACE: Bytes16 = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02];
pub const CTRL_LINEAGE: Bytes16 = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x03];
pub const SYS_SPACE: Bytes16 = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x04];

/// The pinned genesis budget default.
pub const GENESIS_BUDGET: Budget = Budget {
    max_ops: 1_000_000,
    max_bytes: 268_435_456,
};

const T0_MS: u64 = 1_752_400_000_000;
const HLC_STEP_MS: u64 = 60_000;

/// The tranche RNG convention (documented, deterministic): a
/// fixture's ChaCha20 key = SHA-256("d0a/tranche/" ‖ name), nonce =
/// SHA-256("d0a/tranche/" ‖ name ‖ "/nonce")[0..12].
fn rng_for(name: &str) -> RecordingRng {
    use sha2::{Digest, Sha256};
    let key: [u8; 32] = Sha256::digest(format!("d0a/tranche/{name}")).into();
    let n: [u8; 32] = Sha256::digest(format!("d0a/tranche/{name}/nonce")).into();
    let nonce: [u8; 12] = n[..12].try_into().expect("12-byte prefix");
    RecordingRng::new(key, nonce)
}

/// `H_cert(certificate bytes)` — the dev-arm `cert` citation.
pub fn h_cert(cert: &Cert) -> Bytes32 {
    h_tag(
        Tag::Cert,
        &cbor::encode(&cert.to_value()).expect("cert encodes"),
    )
}

/// `H_grant(grant bytes)` — the dev-arm `cap` citation.
pub fn h_grant(grant: &Grant) -> Bytes32 {
    h_tag(
        Tag::Grant,
        &cbor::encode(&grant.to_value()).expect("grant encodes"),
    )
}

/// The trusted-plane 15-verb genesis set: every verb except reserved
/// `admin` and system-only `audit.write` (D-76), in registry order.
fn trusted_verbs() -> Vec<Verb> {
    vec![
        Verb::Search,
        Verb::Read,
        Verb::EvidenceRead,
        Verb::Propose,
        Verb::Assert,
        Verb::JudgeSafe,
        Verb::JudgeFull,
        Verb::PinSafe,
        Verb::PinFull,
        Verb::EraseRequest,
        Verb::Raise,
        Verb::Declassify,
        Verb::Export,
        Verb::Import,
        Verb::CurateInstruction,
    ]
}

/// −P and its scalar: the SEC1 point negation of a P-256 key and the
/// matching secret n − d — derivable by ANY holder of d, which is
/// exactly D-190's residual (freshness equivalence is exact SEC1
/// bytes; negation is deliberately NOT detected).
pub fn negate_p256(pk: &[u8; 65], sk: &[u8; 32]) -> ([u8; 65], [u8; 32]) {
    use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
    use p256::elliptic_curve::PrimeField;
    let ep = p256::EncodedPoint::from_bytes(&pk[..]).expect("SEC1 parses");
    let aff = p256::AffinePoint::from_encoded_point(&ep).expect("point on curve");
    let neg = (-p256::ProjectivePoint::from(aff)).to_affine();
    let neg_pk: [u8; 65] = neg
        .to_encoded_point(false)
        .as_bytes()
        .try_into()
        .expect("uncompressed SEC1");
    let d = p256::Scalar::from_repr((*sk).into()).expect("scalar in range");
    let neg_sk: [u8; 32] = (-d).to_repr().into();
    (neg_pk, neg_sk)
}

/// Draw a 16-byte real ID (N1: the first 8 bytes must not be all
/// zero — astronomically improbable from the stream; asserted).
fn draw_id(rng: &mut RecordingRng, name: &str) -> Bytes16 {
    let id = rng.draw16(name);
    assert!(
        id[..8].iter().any(|b| *b != 0),
        "drawn ID landed in the N1 reserved range: {name}"
    );
    id
}

/// One enrolled device's key material and certificate.
pub struct Device {
    pub device_id: Bytes16,
    pub lineage: Bytes16,
    pub revocation_id: Bytes16,
    pub sig_sk: ed25519_dalek::SigningKey,
    pub sig_pk: [u8; 32],
    pub kem_sk: [u8; 32],
    pub kem_pk: [u8; 65],
    pub cert: Cert,
}

impl Device {
    /// O8: `human`/`daemon`/`browser`/`service` actor ids are the
    /// lowercase hex of the signing device's `device_id`.
    fn actor(&self) -> Actor {
        Actor {
            kind: ActorKind::Daemon,
            id: hex(&self.device_id),
            attested_by: None,
        }
    }
}

/// Draw a device: ed25519 signing keypair, P-256 KEM keypair
/// (RFC 9180 DeriveKeyPair — or the caller-supplied pair, in which
/// case no KEM ikm is drawn), ids, and its daemon-class certificate.
fn mint_device_inner(
    rng: &mut RecordingRng,
    tag: &str,
    plane_id: Bytes32,
    kem: Option<([u8; 32], [u8; 65])>,
) -> Device {
    let sig_seed = rng.draw32(&format!("{tag}.sig_seed"));
    let (sig_sk, sig_pk) = suite::ed25519::keypair(&sig_seed);
    let (kem_sk, kem_pk) = kem.unwrap_or_else(|| {
        let kem_ikm = rng.draw32(&format!("{tag}.kem_ikm"));
        hpke_wrap::derive_keypair(&kem_ikm)
    });
    let device_id = draw_id(rng, &format!("{tag}.device_id"));
    let lineage = draw_id(rng, &format!("{tag}.lineage"));
    let revocation_id = draw_id(rng, &format!("{tag}.revocation_id"));
    let evidence_hash = rng.draw32(&format!("{tag}.evidence_hash"));
    let cert = Cert {
        plane_id,
        device_id,
        sig_alg: Sigalg::Ed25519,
        sig_pk: sig_pk.to_vec(),
        kem_pk: kem_pk.to_vec(),
        class: Devclass::Daemon,
        evidence_hash,
        evidence_media_type: None,
        issued_admin_epoch: 1,
        expiry_deadline_ms: None,
        revocation_id,
        renews: None,
    };
    Device {
        device_id,
        lineage,
        revocation_id,
        sig_sk,
        sig_pk,
        kem_sk,
        kem_pk,
        cert,
    }
}

/// A deterministic single-owner trusted plane: the genesis ceremony
/// minted from named draws, plus helpers that keep the control-chain
/// arithmetic (dense seq; `previous_writer_hash` = gen_start at seq 1,
/// else the predecessor's op hash) and the O7 conventions correct by
/// construction.
pub struct PlaneRig {
    pub rng: RecordingRng,
    hlc_ms: u64,
    pub plane_id: Bytes32,
    pub zone_id: Bytes16,
    pub home_space: Bytes16,
    pub audit_space: Bytes16,
    pub kek_e1: [u8; 32],
    pub root_sk: ed25519_dalek::SigningKey,
    pub root_pk: [u8; 32],
    ctrl_seq: u64,
    ctrl_head: Bytes32,
    pub dev1: Device,
    pub genesis_grant: Grant,
    pub genesis_wrap: Kekwrap,
    pub genesis_op: Signedop,
}

impl PlaneRig {
    /// Mint the plane: the full `c.genesis` ceremony per the §7.1
    /// registry row + D-68/D-76 cross-field validity.
    pub fn new(fixture_name: &str) -> PlaneRig {
        let mut rng = rng_for(fixture_name);
        let mut hlc_ms = T0_MS;

        let root_seed = rng.draw32("root.sig_seed");
        let (root_sk, root_pk) = suite::ed25519::keypair(&root_seed);
        let recovery_seed = rng.draw32("recovery.sig_seed");
        let (_recovery_sk, recovery_pk) = suite::ed25519::keypair(&recovery_seed);

        let descriptor = Genesis {
            root_sig_alg: Sigalg::Ed25519,
            root_sig_pk: root_pk.to_vec(),
            recovery_commitment: h_tag(Tag::Drill, &recovery_pk),
            provenance: Provenance::Trusted,
            created_ms: T0_MS,
        };
        let plane_id = h_tag(
            Tag::Genesis,
            &cbor::encode(&descriptor.to_value()).expect("descriptor encodes"),
        );

        let dev1 = mint_device_inner(&mut rng, "dev1", plane_id, None);
        let zone_id = draw_id(&mut rng, "zone_id");
        let home_space_id = draw_id(&mut rng, "home.space_id");
        let home_name_hash = rng.draw32("home.name_hash");
        let audit_space_id = draw_id(&mut rng, "audit.space_id");
        let audit_name_hash = rng.draw32("audit.name_hash");
        let kek_e1 = rng.draw32("kek.zone.e1");

        let wrap1_eph = rng.draw32("wrap.dev1.eph");
        let (enc, ct) =
            keyschedule::wrap_kek(&dev1.kem_pk, &plane_id, &zone_id, 1, &kek_e1, &wrap1_eph)
                .expect("derived recipient key is well-formed");
        let wrap1 = Kekwrap {
            plane_id,
            zone_id,
            epoch: 1,
            recipient_device: dev1.device_id,
            recipient_kem_key: suite::key_id("hpke-p256-v1", &dev1.kem_pk),
            enc,
            ct,
        };

        let grant = Grant {
            plane_id,
            grant_id: draw_id(&mut rng, "grant1.grant_id"),
            subject_device: dev1.device_id,
            lineage: Some(dev1.lineage),
            tenants: vec![GrantTenant::Memory],
            zone: ZoneSel::Zone(zone_id),
            spaces: SpacesSel::Spaces(vec![home_space_id]),
            ops: trusted_verbs(),
            kinds: None,
            class_ceiling: Class::Sensitive,
            can_declassify: None,
            can_raise: None,
            raise_quota: None,
            flows: None,
            budget: Some(GENESIS_BUDGET),
            online_lease: false,
            max_age_ms: None,
            issued_admin_epoch: 1,
            capability_epoch: 1,
            expiry_deadline_ms: None,
        };
        let audit_grant = Grant {
            grant_id: draw_id(&mut rng, "audit_grant1.grant_id"),
            spaces: SpacesSel::Spaces(vec![audit_space_id]),
            ops: vec![Verb::AuditWrite],
            ..grant.clone()
        };

        let body = Cgenesis {
            descriptor,
            cert: dev1.cert.clone(),
            lineage: Lineagedef {
                lineage: dev1.lineage,
                device_id: dev1.device_id,
                max_generations: 8,
            },
            zone_id,
            zone_wraps: vec![wrap1.clone()],
            home_space: Spacedef {
                space_id: home_space_id,
                zone_id,
                name_hash: home_name_hash,
                space_class: Spaceclass::Personal,
                class_minimum: Class::Private,
                status_policy: Polref {
                    id: "workflow-v1".into(),
                    version: 1,
                    hash: scenario::workflow_v1().hash(),
                },
            },
            audit_space: Spacedef {
                space_id: audit_space_id,
                zone_id,
                name_hash: audit_name_hash,
                space_class: Spaceclass::Audit,
                class_minimum: Class::Private,
                status_policy: Polref {
                    id: "owner-v1".into(),
                    version: 1,
                    hash: scenario::owner_v1().hash(),
                },
            },
            zone_policy: scenario::genesis_zone_policy(zone_id),
            grant: grant.clone(),
            audit_grant,
        };

        // Seal `c.genesis`: control seq 1, genesis arm, root-signed.
        let request_id = draw_id(&mut rng, "ctrl1.request_id");
        hlc_ms += HLC_STEP_MS;
        let header = ctrl_header(
            plane_id,
            CTRL_ZONE,
            CTRL_SPACE,
            Sigalg::Ed25519,
            suite::key_id("ed25519", &root_pk),
            Writer {
                lineage: CTRL_LINEAGE,
                gen: 1,
            },
            Authproof::Genesis { genesis: plane_id },
            request_id,
            1,
            None,
            Hlc {
                ms: hlc_ms,
                count: 0,
            },
            Cgenesis::OP_TYPE,
        );
        let genesis_op = seal_op(header, body.to_value(), &OpSigner::Ed25519(&root_sk));
        assert!(genesis_op.verify(&root_pk), "genesis must verify");
        let ctrl_head = genesis_op.op_hash();

        PlaneRig {
            rng,
            hlc_ms,
            plane_id,
            zone_id,
            home_space: home_space_id,
            audit_space: audit_space_id,
            kek_e1,
            root_sk,
            root_pk,
            ctrl_seq: 1,
            ctrl_head,
            dev1,
            genesis_grant: grant,
            genesis_wrap: wrap1,
            genesis_op,
        }
    }

    fn next_hlc(&mut self) -> Hlc {
        self.hlc_ms += HLC_STEP_MS;
        Hlc {
            ms: self.hlc_ms,
            count: 0,
        }
    }

    /// Draw a device (see [`mint_device_inner`]).
    pub fn mint_device(&mut self, tag: &str) -> Device {
        mint_device_inner(&mut self.rng, tag, self.plane_id, None)
    }

    /// Draw a device whose KEM key is the NEGATION of the given pair
    /// (D-190): fresh signing key and ids, `kem_pk = −P`,
    /// `kem_sk = n − d`. No KEM ikm is drawn — the key material is
    /// derived, which is the point of the residual.
    pub fn mint_device_negated(&mut self, tag: &str, of_pk: [u8; 65], of_sk: [u8; 32]) -> Device {
        let (neg_pk, neg_sk) = negate_p256(&of_pk, &of_sk);
        mint_device_inner(&mut self.rng, tag, self.plane_id, Some((neg_sk, neg_pk)))
    }

    /// An epoch-1 wrap of the zone KEK to `dev`.
    pub fn wrap_to(&mut self, dev: &Device, draw: &str) -> Kekwrap {
        let kek = self.kek_e1;
        self.wrap_at(dev.device_id, &dev.kem_pk.clone(), 1, &kek, draw)
    }

    /// A wrap of `kek` at `epoch` to the given recipient (rotations
    /// mint fresh KEKs at `new_epoch = current + 1`).
    pub fn wrap_at(
        &mut self,
        device_id: Bytes16,
        kem_pk: &[u8; 65],
        epoch: u64,
        kek: &[u8; 32],
        draw: &str,
    ) -> Kekwrap {
        let eph = self.rng.draw32(draw);
        let (enc, ct) =
            keyschedule::wrap_kek(kem_pk, &self.plane_id, &self.zone_id, epoch, kek, &eph)
                .expect("derived recipient key is well-formed");
        Kekwrap {
            plane_id: self.plane_id,
            zone_id: self.zone_id,
            epoch,
            recipient_device: device_id,
            recipient_kem_key: suite::key_id("hpke-p256-v1", kem_pk),
            enc,
            ct,
        }
    }

    /// A minimal op-authoring grant on the genesis zone's home space.
    pub fn simple_grant(&mut self, tag: &str, dev: &Device, ops: Vec<Verb>) -> Grant {
        Grant {
            plane_id: self.plane_id,
            grant_id: draw_id(&mut self.rng, &format!("{tag}.grant_id")),
            subject_device: dev.device_id,
            lineage: Some(dev.lineage),
            tenants: vec![GrantTenant::Memory],
            zone: ZoneSel::Zone(self.zone_id),
            spaces: SpacesSel::Spaces(vec![self.home_space]),
            ops,
            kinds: None,
            class_ceiling: Class::Sensitive,
            can_declassify: None,
            can_raise: None,
            raise_quota: None,
            flows: None,
            budget: Some(GENESIS_BUDGET),
            online_lease: false,
            max_age_ms: None,
            issued_admin_epoch: 1,
            capability_epoch: 1,
            expiry_deadline_ms: None,
        }
    }

    /// Seal the next control operation on the dense chain.
    fn seal_ctrl(&mut self, op_type: &str, proof: Authproof, body: cbor::Value) -> Signedop {
        let seq = self.ctrl_seq + 1;
        let request_id = draw_id(&mut self.rng, &format!("ctrl{seq}.request_id"));
        let hlc = self.next_hlc();
        let header = ctrl_header(
            self.plane_id,
            CTRL_ZONE,
            CTRL_SPACE,
            Sigalg::Ed25519,
            suite::key_id("ed25519", &self.root_pk),
            Writer {
                lineage: CTRL_LINEAGE,
                gen: 1,
            },
            proof,
            request_id,
            seq,
            Some(self.ctrl_head),
            hlc,
            op_type,
        );
        let op = seal_op(header, body, &OpSigner::Ed25519(&self.root_sk));
        assert!(op.verify(&self.root_pk), "control op must verify");
        self.ctrl_seq = seq;
        self.ctrl_head = op.op_hash();
        op
    }

    /// `c.enroll` (new-device shape) for `dev` with `grants`,
    /// carrying an epoch-1 zone wrap — admin arm at epoch 1 (the
    /// root key IS the epoch-1 admin key, O7).
    pub fn enroll_new(&mut self, dev: &Device, grants: Vec<Grant>, wrap_draw: &str) -> Signedop {
        let wraps = vec![self.wrap_to(dev, wrap_draw)];
        let body = Cenrollnew {
            cert: dev.cert.clone(),
            grants,
            lineage: Lineagedef {
                lineage: dev.lineage,
                device_id: dev.device_id,
                max_generations: 8,
            },
            wraps,
        };
        let proof = Authproof::Admin {
            epoch: 1,
            ctrl_frontier: self.ctrl_head,
        };
        self.seal_ctrl(Cenrollnew::OP_TYPE, proof, body.to_value())
    }

    /// `c.grant` — issue one grant (admin arm).
    pub fn grant_op(&mut self, grant: Grant) -> Signedop {
        let proof = Authproof::Admin {
            epoch: 1,
            ctrl_frontier: self.ctrl_head,
        };
        self.seal_ctrl(Cgrant::OP_TYPE, proof, Cgrant { grant }.to_value())
    }

    /// `c.revoke_device` (exclude mode) targeting `target`'s
    /// `revocation_id`, with the given authorship-domain cutoffs and
    /// no rotation references (legal on a trusted plane — references
    /// are typed linkage, never coverage; D-180/D-195).
    pub fn revoke_device_exclude(
        &mut self,
        target: &Device,
        cutoffs: Vec<Frontierclose>,
    ) -> Signedop {
        let body = Crevokedev {
            mode: RevokeMode::Exclude,
            revocation_id: target.revocation_id,
            cutoffs,
            receipt_cutoffs: None,
            rotation_refs: vec![],
        };
        let proof = Authproof::Admin {
            epoch: 1,
            ctrl_frontier: self.ctrl_head,
        };
        self.seal_ctrl(Crevokedev::OP_TYPE, proof, body.to_value())
    }

    /// `c.kek_rotate` to `new_epoch` with the given wraps and an
    /// empty erase manifest.
    pub fn kek_rotate(&mut self, new_epoch: u64, wraps: Vec<Kekwrap>) -> Signedop {
        let body = Ckekrotate {
            zone_id: self.zone_id,
            new_epoch,
            wraps,
            erase_manifest: vec![],
        };
        let proof = Authproof::Admin {
            epoch: 1,
            ctrl_frontier: self.ctrl_head,
        };
        self.seal_ctrl(Ckekrotate::OP_TYPE, proof, body.to_value())
    }

    /// A tenant `m.claim` (plain propose) by `dev` under `grant`, at
    /// `(gen 1, writer_sequence)` of the device's lineage.
    pub fn claim(
        &mut self,
        dev: &Device,
        grant: &Grant,
        tag: &str,
        statement: &str,
        writer_sequence: u64,
        previous_writer_hash: Option<Bytes32>,
    ) -> Signedop {
        let body = Mclaim {
            kind: Kind::Observation,
            statement: statement.into(),
            sensitivity: Class::Private,
            observed_at_ms: Some(self.hlc_ms),
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
        let request_id = draw_id(&mut self.rng, &format!("{tag}.request_id"));
        let hlc = self.next_hlc();
        let header = Header {
            tenant: Tenant::Memory,
            plane_id: self.plane_id,
            zone_id: self.zone_id,
            space_id: self.home_space,
            authored_kek_epoch: 1,
            capability_epoch: 1,
            signer_alg: Sigalg::Ed25519,
            signer_key_id: suite::key_id("ed25519", &dev.sig_pk),
            writer: Writer {
                lineage: dev.lineage,
                gen: 1,
            },
            actor: dev.actor(),
            authorization_proof: Authproof::Dev {
                cert: h_cert(&dev.cert),
                cap: h_grant(grant),
            },
            request_id,
            writer_sequence,
            previous_writer_hash: previous_writer_hash
                .unwrap_or_else(|| gen_start(&dev.lineage, 1)),
            causal_references: vec![],
            created_hlc: hlc,
            operation_type: Mclaim::OP_TYPE.into(),
            operation_version: 1,
            body_hash: [0; 32], // set by seal_op
        };
        let op = seal_op(header, body.to_value(), &OpSigner::Ed25519(&dev.sig_sk));
        assert!(op.verify(&dev.sig_pk), "tenant op must verify");
        op
    }
}

fn items(entries: &[(&str, &Signedop)]) -> Json {
    let mut m = JsonMap::new();
    for (name, op) in entries {
        m.insert((*name).into(), json!(hex(&op.encode())));
    }
    Json::Object(m)
}

fn admits(item: &str) -> Json {
    json!({ "item": item })
}

/// Tranche #2 — family 7 fold: C1→I→C2 vs C1→C2→I delayed-reference
/// convergence (D-199): `i` cites a certificate and grant that ride
/// `c2`; delivered before `c2` it pends `ref-unresolved` (an unheld
/// reference is never proven absent — D-194 withdrawn), and admits
/// when `c2` arrives. Both orders and the fresh fold of the union
/// reach the same all-admitted state.
pub fn f7_delayed_reference_convergence() -> Vector {
    let name = "delayed-reference-convergence-c1-i-c2";
    let mut rig = PlaneRig::new(name);

    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
    let i = rig.claim(
        &dev2,
        &grant2,
        "i",
        "harbor water level 2.1 m at the morning reading",
        1,
        None,
    );

    let c1 = &rig.genesis_op;
    let mut inputs = JsonMap::new();
    inputs.insert("items".into(), items(&[("c1", c1), ("c2", &c2), ("i", &i)]));
    inputs.insert(
        "deliveries".into(),
        json!([["c1", "i", "c2"], ["c1", "c2", "i"]]),
    );

    Vector {
        family: 7,
        name: name.into(),
        case_kind: "fold".into(),
        source: "10.2".into(),
        surfaces: vec!["core".into()],
        rng: Some(rig.rng.into_json()),
        inputs,
        expected: Expected::Result(json!({
            "per_item": [admits("c1"), admits("c2"), admits("i")],
            "converge": true,
            "trace": [{
                "delivery": 0,
                "after": "i",
                "item": "i",
                "outcome": "ref-unresolved",
                "disposition": "pending-dependency",
            }],
        })),
    }
}

/// Tranche #8 — family 7 fold: negation-residual acceptance (D-190):
/// dev1's KEM point P is enrolled at genesis; a candidate device
/// enrolls with `kem_pk = −P` (and a fresh signing key). The
/// freshness domain compares EXACT SEC1 bytes — `−P ≠ P`, `mat_id`
/// differs — so the enrollment ADMITS. The residual is deliberate
/// (§14): a holder of scalar d derives −P's scalar, and no public
/// identifier can detect the relation; the fixture's internals test
/// demonstrates exactly that derivation opening the new wrap.
pub fn f7_negation_residual() -> Vector {
    let name = "negation-residual-acceptance";
    let mut rig = PlaneRig::new(name);

    let (p, d) = (rig.dev1.kem_pk, rig.dev1.kem_sk);
    let dev2 = rig.mint_device_negated("dev2", p, d);
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2], "wrap.dev2.eph");

    let c1 = &rig.genesis_op;
    let mut inputs = JsonMap::new();
    inputs.insert("items".into(), items(&[("c1", c1), ("c2", &c2)]));
    inputs.insert("deliveries".into(), json!([["c1", "c2"]]));

    Vector {
        family: 7,
        name: name.into(),
        case_kind: "fold".into(),
        source: "7.1".into(),
        surfaces: vec!["core".into()],
        rng: Some(rig.rng.into_json()),
        inputs,
        expected: Expected::Result(json!({
            "per_item": [admits("c1"), admits("c2")],
            "converge": true,
        })),
    }
}

/// Tranche #5 — family 7 fold: pending `c.revoke_device` → grant
/// issued in the window → completing continuation (D-195). The
/// compound `r` covers dev2's authorship domain (one zone, empty
/// heads — nothing authored) but dev2's decryptable-wrap domain is
/// nonempty, so `r` pends `ref-unresolved`. `g` issues dev2 a grant
/// in the window — the certificate ceases only at the COMPLETING
/// acceptance's position, so `g` was issued while it was effective
/// and ADMITS. `k` (an epoch-2 rotation excluding dev2) empties the
/// wrap domain: cessation = `k`'s position. The out-of-order delivery
/// additionally pins §9.3's chain arithmetic on the CONTROL chain: a
/// successor citing an unknown predecessor is `causal-missing`.
pub fn f7_pending_revocation_window_grant() -> Vector {
    let name = "pending-revocation-window-grant-completing-rotation";
    let mut rig = PlaneRig::new(name);

    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2], "wrap.dev2.eph");

    // r: authorship cutoffs total (the zone, empty heads — D-143);
    // wrap domain nonempty (the epoch-1 wrap) → pends.
    let r = {
        let cutoff = Frontierclose {
            zone_id: rig.zone_id,
            lineage: dev2.lineage,
            heads: vec![],
        };
        rig.revoke_device_exclude(&dev2, vec![cutoff])
    };

    // g: a window-issued grant to the pending-revocation device.
    let grant3 = rig.simple_grant("grant3", &dev2, vec![Verb::Propose]);
    let g = rig.grant_op(grant3);

    // k: the completing exclusion — fresh epoch-2 KEK wrapped to dev1
    // only (the last-holder precondition retains a recipient).
    let k = {
        let kek_e2 = rig.rng.draw32("kek.zone.e2");
        let (d1_id, d1_pk) = (rig.dev1.device_id, rig.dev1.kem_pk);
        let w = rig.wrap_at(d1_id, &d1_pk, 2, &kek_e2, "wrap.dev1.e2.eph");
        rig.kek_rotate(2, vec![w])
    };

    let c1 = &rig.genesis_op;
    let mut inputs = JsonMap::new();
    inputs.insert(
        "items".into(),
        items(&[("c1", c1), ("c2", &c2), ("r", &r), ("g", &g), ("k", &k)]),
    );
    inputs.insert(
        "deliveries".into(),
        json!([["c1", "c2", "r", "g", "k"], ["c1", "c2", "g", "k", "r"],]),
    );

    Vector {
        family: 7,
        name: name.into(),
        case_kind: "fold".into(),
        source: "7.1".into(),
        surfaces: vec!["core".into()],
        rng: Some(rig.rng.into_json()),
        inputs,
        expected: Expected::Result(json!({
            "per_item": [
                admits("c1"),
                admits("c2"),
                admits("r"),
                admits("g"),
                admits("k"),
            ],
            "converge": true,
            "trace": [
                {
                    "delivery": 0,
                    "after": "r",
                    "item": "r",
                    "outcome": "ref-unresolved",
                    "disposition": "pending-dependency",
                },
                {
                    "delivery": 1,
                    "after": "g",
                    "item": "g",
                    "outcome": "causal-missing",
                    "disposition": "pending-dependency",
                },
                {
                    "delivery": 1,
                    "after": "k",
                    "item": "k",
                    "outcome": "causal-missing",
                    "disposition": "pending-dependency",
                },
            ],
        })),
    }
}

/// Every tranche fixture, in annex order (grows as builders land).
pub fn tranche() -> Vec<Vector> {
    vec![
        f7_delayed_reference_convergence(),
        f7_negation_residual(),
        f7_pending_revocation_window_grant(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shapes::assert_pins;

    const SPEC_PINS: &[&str] = &[
        // N1 — the reserved-constant block.
        r#"  CTRL_ZONE    = h'00000000000000000000000000000001'
  CTRL_SPACE   = h'00000000000000000000000000000002'
  CTRL_LINEAGE = h'00000000000000000000000000000003'
  SYS_SPACE    = h'00000000000000000000000000000004'   # w.gen space, §9.3"#,
        "The control chain uses these constants with `gen = 1` always",
        // Chain arithmetic (§9.3, D-68).
        r#"- **Chain arithmetic (exact, D-68)**: within `(zone, lineage, gen)`,
  `writer_sequence` starts at 1 and increments by exactly 1;
  `previous_writer_hash` = `gen_start(lineage, gen)` at seq 1, else
  the op hash of seq − 1."#,
        // D-199 — the convergence rule this fixture pins.
        "unresolved references PEND
         (D-199): a cited certificate or grant not yet held is
         `ref-unresolved` — indefinitely if need be",
        // O7/O8 conventions the rig implements.
        "genesis operations are signed by the
  descriptor's root key; admin operations by the **current admin key
  at their epoch** (the root key IS the epoch-1 admin key;",
        "`human`, `daemon`, `browser`, `service` → the lowercase hex of the
  signing device's `device_id`",
        // The genesis budget default.
        "pinned genesis default `{max_ops: 1000000, max_bytes: 268435456}`",
        // The dev-arm citation identities.
        r#"authproof = { arm: "dev", cert: bytes32, cap: bytes32 }
            ; cert = H_cert(certificate bytes);
            ;   cap = H_grant(grant bytes) (D-77)"#,
        // D-190 — the negation residual this tranche pins.
        "**P-vs-−P negation reuse** (exact-SEC1 equality is deliberate — a holder of scalar d derives −P's scalar, and negation is NOT detected; D-190)",
        "all under EXACT-SEC1 identity — −P and related keys are outside the equivalence, stated §14 residuals, D-190",
    ];

    #[test]
    fn spec_pins_are_verbatim() {
        assert_pins(SPEC_PINS);
    }

    fn companion() -> Json {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("d0a-vector-cases.v1.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Every tranche vector passes the mint-time conformance check.
    #[test]
    fn tranche_vectors_check_green() {
        let c = companion();
        for v in tranche() {
            crate::vector::check(&v.to_json(), &c).unwrap_or_else(|e| panic!("{}: {e}", v.name));
        }
    }

    /// The committed vector files match the builders byte-for-byte
    /// (regenerate with `cargo run --bin mint`).
    #[test]
    fn committed_vectors_match_builders() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vectors");
        for v in tranche() {
            let path = dir.join(format!("f{:02}-{}.json", v.family, v.name));
            let committed = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!(
                    "missing committed vector {} ({e}) — run `cargo run --bin mint`",
                    path.display()
                )
            });
            assert_eq!(
                committed,
                v.to_file_string(),
                "committed vector drifted from its builder: {}",
                v.name
            );
        }
    }

    /// The rig's determinism: two mints of one fixture are
    /// byte-identical.
    #[test]
    fn builders_are_deterministic() {
        let a = f7_delayed_reference_convergence().to_file_string();
        let b = f7_delayed_reference_convergence().to_file_string();
        assert_eq!(a, b);
    }

    /// Fixture-internal semantics: the ops really verify, the chain
    /// arithmetic holds, the claim's citations resolve to the
    /// enrollment's carried objects, and the enrollment wrap opens
    /// to the device's KEM key.
    #[test]
    fn f7_delayed_reference_internals() {
        let name = "delayed-reference-convergence-c1-i-c2";
        let mut rig = PlaneRig::new(name);
        let dev2 = rig.mint_device("dev2");
        let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
        let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");
        let i = rig.claim(&dev2, &grant2, "i", "x", 1, None);
        let c1 = &rig.genesis_op;

        // Control chain: dense seq from 1, previous = predecessor.
        assert_eq!(c1.header.writer_sequence, 1);
        assert_eq!(c1.header.previous_writer_hash, gen_start(&CTRL_LINEAGE, 1));
        assert_eq!(c2.header.writer_sequence, 2);
        assert_eq!(c2.header.previous_writer_hash, c1.op_hash());

        // O7 pins on both control ops.
        for op in [c1, &c2] {
            assert_eq!(op.header.authored_kek_epoch, 0);
            assert_eq!(op.header.capability_epoch, 0);
            assert_eq!(op.header.zone_id, CTRL_ZONE);
            assert_eq!(op.header.space_id, CTRL_SPACE);
            assert_eq!(op.header.writer.lineage, CTRL_LINEAGE);
            assert_eq!(op.header.writer.gen, 1);
        }

        // The claim's dev-arm citations are the hashes of the cert
        // and grant carried by c2; the tenant chain opens at seq 1.
        let Authproof::Dev { cert, cap } = i.header.authorization_proof else {
            panic!("claim must use the dev arm");
        };
        assert_eq!(cert, h_cert(&dev2.cert));
        assert_eq!(cap, h_grant(&grant2));
        assert_eq!(i.header.writer_sequence, 1);
        assert_eq!(i.header.previous_writer_hash, gen_start(&dev2.lineage, 1));
        assert_eq!(i.header.actor.id, hex(&dev2.device_id));

        // The wrap minted for dev2 (re-derived deterministically on a
        // fresh rig) opens to dev2's KEM secret.
        let mut rig2 = PlaneRig::new(name);
        let dev2b = rig2.mint_device("dev2");
        let _ = rig2.simple_grant("grant2", &dev2b, vec![Verb::Propose]);
        let w = rig2.wrap_to(&dev2b, "wrap.dev2.eph");
        assert_eq!(
            keyschedule::open_kek(
                &dev2b.kem_sk,
                &rig2.plane_id,
                &rig2.zone_id,
                1,
                &w.enc,
                &w.ct
            ),
            Some(rig2.kek_e1)
        );

        // And the genesis wrap opens to dev1.
        let w1 = &rig.genesis_wrap;
        assert_eq!(
            keyschedule::open_kek(
                &rig.dev1.kem_sk,
                &rig.plane_id,
                &rig.zone_id,
                1,
                &w1.enc,
                &w1.ct
            ),
            Some(rig.kek_e1)
        );
    }

    /// D-190 internals: the negated certificate really carries −P
    /// (distinct SEC1 bytes, distinct key_id and mat_id — outside the
    /// freshness equivalence), and the scalar n − d that any holder
    /// of d can derive opens the wrap addressed to −P — the stated
    /// residual, demonstrated.
    #[test]
    fn f7_negation_residual_internals() {
        use crate::shapes::envelope::mat_id;
        let name = "negation-residual-acceptance";
        let mut rig = PlaneRig::new(name);
        let (p, d) = (rig.dev1.kem_pk, rig.dev1.kem_sk);
        let dev2 = rig.mint_device_negated("dev2", p, d);

        // −P is a different SEC1 point with different identifiers…
        assert_ne!(dev2.kem_pk, p);
        assert_eq!(dev2.kem_pk[1..33], p[1..33], "same X coordinate");
        assert_ne!(dev2.kem_pk[33..], p[33..], "negated Y coordinate");
        assert_ne!(
            suite::key_id("hpke-p256-v1", &dev2.kem_pk),
            suite::key_id("hpke-p256-v1", &p)
        );
        assert_ne!(mat_id(&dev2.kem_pk), mat_id(&p));

        // …and the derived scalar opens a wrap addressed to it.
        let w = rig.wrap_to(&dev2, "wrap.dev2.eph");
        assert_eq!(
            keyschedule::open_kek(&dev2.kem_sk, &rig.plane_id, &rig.zone_id, 1, &w.enc, &w.ct),
            Some(rig.kek_e1)
        );
        // dev1's own scalar does NOT open it (it is a real distinct key).
        assert_eq!(
            keyschedule::open_kek(&d, &rig.plane_id, &rig.zone_id, 1, &w.enc, &w.ct),
            None
        );
    }
}
