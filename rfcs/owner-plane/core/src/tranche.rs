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
    ctrl_header, AdminKey, Ccutoff, Cdrill, Cenrollnew, Cepochbump, Cgenesis, Cgrant, Ckekrotate,
    Crecovsucc, Crevokedev, Crevokegrant, Czonecreate, Czonepolicy, RevokeMode,
    CSPACECREATE_OP_TYPE,
};
use crate::shapes::envelope::{
    gen_start, seal_op, Actor, ActorKind, Header, OpSigner, Signedop, Tenant, Writer,
};
use crate::shapes::identity::Flow;
use crate::shapes::identity::{
    Authproof, Budget, Cert, Endpoint, Genesis, Grant, GrantTenant, Provenance, SpacesSel, ZoneSel,
};
use crate::shapes::journal::AbortReason;
use crate::shapes::journal::{
    sign_lease, sign_receipt, Frontier, Itemcommit, Itemwrap, Leasestmt, MissingRec, Pendingxfer,
    Receiptstmt, Signedstmt, Txn, Txnrec, Xferabort, Xferdone, Xferreopen,
};
use crate::shapes::memory::{
    merkle_root, Bundleleaf, Bundlerec, Mclaim, Merasereq, Mexportrel, Mimport,
};
use crate::shapes::{
    Bytes16, Bytes32, Class, Devclass, Factref, Frontierclose, Head, Hlc, Issuerid, Kekwrap, Kind,
    Lineagedef, Opfactref, Polref, Sigalg, Spaceclass, Spacedef, ToValue, Verb,
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

pub(crate) const T0_MS: u64 = 1_752_400_000_000;
const HLC_STEP_MS: u64 = 60_000;

/// The tranche RNG convention (documented, deterministic): a
/// fixture's ChaCha20 key = SHA-256("d0a/tranche/" ‖ name), nonce =
/// SHA-256("d0a/tranche/" ‖ name ‖ "/nonce")[0..12].
pub(crate) fn rng_for(name: &str) -> RecordingRng {
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
/// The §7.5 hosted-safe verb set, §11.1 order.
fn hosted_safe_verbs() -> Vec<Verb> {
    vec![
        Verb::Search,
        Verb::Read,
        Verb::EvidenceRead,
        Verb::Propose,
        Verb::Assert,
        Verb::JudgeSafe,
        Verb::PinSafe,
        Verb::EraseRequest,
        Verb::Raise,
    ]
}

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
pub(crate) fn draw_id(rng: &mut RecordingRng, name: &str) -> Bytes16 {
    let id = rng.draw16(name);
    assert!(
        id[..8].iter().any(|b| *b != 0),
        "drawn ID landed in the N1 reserved range: {name}"
    );
    id
}

/// One enrolled device's key material and certificate.
#[derive(Clone)]
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

/// Draw a device: ed25519 signing keypair, P-256 KEM keypair
/// (RFC 9180 DeriveKeyPair — or the caller-supplied pair, in which
/// case no KEM ikm is drawn), ids, and its daemon-class certificate.
fn mint_device_inner(
    rng: &mut RecordingRng,
    tag: &str,
    plane_id: Bytes32,
    kem: Option<([u8; 32], [u8; 65])>,
    class: Devclass,
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
        class,
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
/// Header overrides for [`PlaneRig::tenant_op_over`].
pub struct TenantOverrides {
    pub actor_id: Option<String>,
    pub capability_epoch: u64,
    pub authored_kek_epoch: u64,
}

impl Default for TenantOverrides {
    fn default() -> Self {
        TenantOverrides {
            actor_id: None,
            capability_epoch: 1,
            authored_kek_epoch: 1,
        }
    }
}

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
    pub recovery_sk: ed25519_dalek::SigningKey,
    pub recovery_pk: [u8; 32],
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
        Self::new_with(fixture_name, Provenance::Trusted)
    }

    /// [`Self::new`] on a HOSTED plane: `provenance = hosted`, a
    /// `hosted-browser` first certificate, and the §7.5 safe verb
    /// set on the genesis grant — the identical draw sequence, so
    /// trusted fixtures reproduce byte-identically.
    pub fn new_hosted(fixture_name: &str) -> PlaneRig {
        Self::new_with(fixture_name, Provenance::Hosted)
    }

    fn new_with(fixture_name: &str, provenance: Provenance) -> PlaneRig {
        let hosted = provenance == Provenance::Hosted;
        let mut rng = rng_for(fixture_name);
        let mut hlc_ms = T0_MS;

        let root_seed = rng.draw32("root.sig_seed");
        let (root_sk, root_pk) = suite::ed25519::keypair(&root_seed);
        let recovery_seed = rng.draw32("recovery.sig_seed");
        let (recovery_sk, recovery_pk) = suite::ed25519::keypair(&recovery_seed);

        let descriptor = Genesis {
            root_sig_alg: Sigalg::Ed25519,
            root_sig_pk: root_pk.to_vec(),
            recovery_commitment: h_tag(Tag::Drill, &recovery_pk),
            provenance,
            created_ms: T0_MS,
        };
        let plane_id = h_tag(
            Tag::Genesis,
            &cbor::encode(&descriptor.to_value()).expect("descriptor encodes"),
        );

        let dev1 = mint_device_inner(
            &mut rng,
            "dev1",
            plane_id,
            None,
            if hosted {
                Devclass::HostedBrowser
            } else {
                Devclass::Daemon
            },
        );
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
            ops: if hosted {
                hosted_safe_verbs()
            } else {
                trusted_verbs()
            },
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
            recovery_sk,
            recovery_pk,
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
        mint_device_inner(&mut self.rng, tag, self.plane_id, None, Devclass::Daemon)
    }

    /// Draw a device whose KEM key is the NEGATION of the given pair
    /// (D-190): fresh signing key and ids, `kem_pk = −P`,
    /// `kem_sk = n − d`. No KEM ikm is drawn — the key material is
    /// derived, which is the point of the residual.
    pub fn mint_device_negated(&mut self, tag: &str, of_pk: [u8; 65], of_sk: [u8; 32]) -> Device {
        let (neg_pk, neg_sk) = negate_p256(&of_pk, &of_sk);
        mint_device_inner(
            &mut self.rng,
            tag,
            self.plane_id,
            Some((neg_sk, neg_pk)),
            Devclass::Daemon,
        )
    }

    /// An epoch-1 wrap of the zone KEK to `dev`.
    pub fn wrap_to(&mut self, dev: &Device, draw: &str) -> Kekwrap {
        let kek = self.kek_e1;
        self.wrap_at(dev.device_id, &dev.kem_pk.clone(), 1, &kek, draw)
    }

    /// A wrap of `kek` at `epoch` to the given recipient (rotations
    /// mint fresh KEKs at `new_epoch = current + 1`), genesis zone.
    pub fn wrap_at(
        &mut self,
        device_id: Bytes16,
        kem_pk: &[u8; 65],
        epoch: u64,
        kek: &[u8; 32],
        draw: &str,
    ) -> Kekwrap {
        let zone_id = self.zone_id;
        self.wrap_in(zone_id, kek, device_id, kem_pk, epoch, draw)
    }

    /// A wrap of `kek` into an arbitrary zone.
    pub fn wrap_in(
        &mut self,
        zone_id: Bytes16,
        kek: &[u8; 32],
        device_id: Bytes16,
        kem_pk: &[u8; 65],
        epoch: u64,
        draw: &str,
    ) -> Kekwrap {
        let eph = self.rng.draw32(draw);
        let (enc, ct) = keyschedule::wrap_kek(kem_pk, &self.plane_id, &zone_id, epoch, kek, &eph)
            .expect("derived recipient key is well-formed");
        Kekwrap {
            plane_id: self.plane_id,
            zone_id,
            epoch,
            recipient_device: device_id,
            recipient_kem_key: suite::key_id("hpke-p256-v1", kem_pk),
            enc,
            ct,
        }
    }

    /// A minimal op-authoring grant on the genesis zone's home space.
    pub fn simple_grant(&mut self, tag: &str, dev: &Device, ops: Vec<Verb>) -> Grant {
        let (zone_id, home) = (self.zone_id, self.home_space);
        self.grant_in(tag, dev, ops, zone_id, vec![home])
    }

    /// A minimal op-authoring grant on an arbitrary zone/space set.
    pub fn grant_in(
        &mut self,
        tag: &str,
        dev: &Device,
        ops: Vec<Verb>,
        zone_id: Bytes16,
        spaces: Vec<Bytes16>,
    ) -> Grant {
        Grant {
            plane_id: self.plane_id,
            grant_id: draw_id(&mut self.rng, &format!("{tag}.grant_id")),
            subject_device: dev.device_id,
            lineage: Some(dev.lineage),
            tenants: vec![GrantTenant::Memory],
            zone: ZoneSel::Zone(zone_id),
            spaces: SpacesSel::Spaces(spaces),
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

    /// Seal a control op at the next chain position with a named
    /// request-id draw; advancing is the caller's choice.
    fn seal_ctrl_at(
        &mut self,
        op_type: &str,
        proof: Authproof,
        body: cbor::Value,
        req_draw: &str,
    ) -> Signedop {
        let seq = self.ctrl_seq + 1;
        let request_id = draw_id(&mut self.rng, req_draw);
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
        op
    }

    /// Seal the next control operation on the dense chain (advances).
    fn seal_ctrl(&mut self, op_type: &str, proof: Authproof, body: cbor::Value) -> Signedop {
        let seq = self.ctrl_seq + 1;
        let op = self.seal_ctrl_at(op_type, proof, body, &format!("ctrl{seq}.request_id"));
        self.ctrl_seq = seq;
        self.ctrl_head = op.op_hash();
        op
    }

    /// Seal a control op the fixture expects the fold to REJECT: it
    /// occupies the next position's coordinates but the chain does
    /// not advance — a failed operation exerts no precedence (D-112),
    /// so the accepted successor legally reuses the position.
    pub fn seal_ctrl_candidate(
        &mut self,
        tag: &str,
        op_type: &str,
        proof: Authproof,
        body: cbor::Value,
    ) -> Signedop {
        self.seal_ctrl_at(op_type, proof, body, &format!("{tag}.request_id"))
    }

    /// `c.enroll` (new-device shape) for `dev` with `grants`,
    /// carrying an epoch-1 zone wrap — admin arm at epoch 1 (the
    /// root key IS the epoch-1 admin key, O7).
    pub fn enroll_new(&mut self, dev: &Device, grants: Vec<Grant>, wrap_draw: &str) -> Signedop {
        let wraps = vec![self.wrap_to(dev, wrap_draw)];
        self.enroll_new_with_wraps(dev, grants, wraps)
    }

    /// [`Self::enroll_new`] with caller-supplied wraps (multi-zone
    /// enrollments).
    pub fn enroll_new_with_wraps(
        &mut self,
        dev: &Device,
        grants: Vec<Grant>,
        wraps: Vec<Kekwrap>,
    ) -> Signedop {
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

    /// `c.cutoff` as a pure staging operation (requesterless, empty
    /// `cutoffs`, non-empty `closes` — D-136): the staged frontiers
    /// are INERT until a consuming advance materializes them.
    pub fn stage_closes(&mut self, closes: Vec<Frontierclose>) -> Signedop {
        let body = Ccutoff {
            cutoffs: vec![],
            closes: Some(closes),
            requester: None,
        };
        let proof = Authproof::Admin {
            epoch: 1,
            ctrl_frontier: self.ctrl_head,
        };
        self.seal_ctrl(Ccutoff::OP_TYPE, proof, body.to_value())
    }

    /// `c.zone_policy` (D-69: acceptance advances the zone's
    /// capability epoch by 1; under strict, `cutoffs` ∪ staged closes
    /// must cover every live lineage).
    pub fn zone_policy_op(
        &mut self,
        policy: crate::shapes::Zonepolicy,
        cutoffs: Vec<Frontierclose>,
    ) -> Signedop {
        let body = Czonepolicy {
            policy,
            cutoffs: if cutoffs.is_empty() {
                None
            } else {
                Some(cutoffs)
            },
        };
        let proof = Authproof::Admin {
            epoch: 1,
            ctrl_frontier: self.ctrl_head,
        };
        self.seal_ctrl(Czonepolicy::OP_TYPE, proof, body.to_value())
    }

    /// A qualified-witness `accept` receipt (§4.7; T2 excludes the
    /// operation's own signer, so `dev` is the witness device).
    pub fn accept_receipt(&mut self, dev: &Device, subject: Bytes32, seen_ms: u64) -> Signedstmt {
        let stmt = Receiptstmt::Accept {
            issuer: Issuerid::Device {
                cert: h_cert(&dev.cert),
            },
            plane_id: self.plane_id,
            zone_id: self.zone_id,
            subject,
            seen_ms,
            issuer_seq: 1,
            prev_stmt: [0; 32],
        };
        sign_receipt(&stmt, &OpSigner::Ed25519(&dev.sig_sk))
    }

    /// A `LeaseStmt` for `(grant_id, lineage)` (§4.7/T5).
    pub fn lease_stmt(
        &mut self,
        dev: &Device,
        grant_id: Bytes16,
        lineage: Bytes16,
        issued_ms: u64,
        expires_ms: u64,
    ) -> Signedstmt {
        let stmt = Leasestmt {
            issuer: Issuerid::Device {
                cert: h_cert(&dev.cert),
            },
            plane_id: self.plane_id,
            zone_id: self.zone_id,
            grant_id,
            lineage,
            issued_ms,
            expires_ms,
            ctrl_frontier: self.ctrl_head,
            issuer_seq: 1,
            prev_stmt: [0; 32],
        };
        sign_lease(&stmt, &OpSigner::Ed25519(&dev.sig_sk))
    }

    /// `c.cap_epoch_bump` with closure cutoffs (advancing form).
    pub fn epoch_bump(&mut self, new_epoch: u64, cutoffs: Vec<Frontierclose>) -> Signedop {
        let body = Cepochbump {
            zone_id: self.zone_id,
            new_epoch,
            cutoffs: Some(cutoffs),
        };
        let proof = Authproof::Admin {
            epoch: 1,
            ctrl_frontier: self.ctrl_head,
        };
        self.seal_ctrl(Cepochbump::OP_TYPE, proof, body.to_value())
    }

    /// A `c.cap_epoch_bump` candidate the fixture expects rejected
    /// (see [`Self::seal_ctrl_candidate`]).
    pub fn epoch_bump_candidate(
        &mut self,
        tag: &str,
        new_epoch: u64,
        cutoffs: Vec<Frontierclose>,
    ) -> Signedop {
        let body = Cepochbump {
            zone_id: self.zone_id,
            new_epoch,
            cutoffs: Some(cutoffs),
        };
        let proof = Authproof::Admin {
            epoch: 1,
            ctrl_frontier: self.ctrl_head,
        };
        self.seal_ctrl_candidate(tag, Cepochbump::OP_TYPE, proof, body.to_value())
    }

    /// A generic tenant operation by `dev` under `grant` on the home
    /// space, at `(gen 1, writer_sequence)` of the device's lineage.
    #[allow(clippy::too_many_arguments)]
    pub fn tenant_op(
        &mut self,
        dev: &Device,
        grant: &Grant,
        tag: &str,
        op_type: &str,
        body: cbor::Value,
        writer_sequence: u64,
        previous_writer_hash: Option<Bytes32>,
    ) -> Signedop {
        self.tenant_op_as(
            ActorKind::Daemon,
            dev,
            grant,
            tag,
            op_type,
            body,
            writer_sequence,
            previous_writer_hash,
        )
    }

    /// [`Self::tenant_op`] with an explicit actor kind — the O8 id is
    /// always the device hex; `human` on an enrolled device with no
    /// `attested_by` is §10.1 shape-1 direct-human evidence.
    #[allow(clippy::too_many_arguments)]
    pub fn tenant_op_as(
        &mut self,
        actor_kind: ActorKind,
        dev: &Device,
        grant: &Grant,
        tag: &str,
        op_type: &str,
        body: cbor::Value,
        writer_sequence: u64,
        previous_writer_hash: Option<Bytes32>,
    ) -> Signedop {
        let (zone_id, space_id) = (self.zone_id, self.home_space);
        self.tenant_op_in(
            zone_id,
            space_id,
            actor_kind,
            dev,
            grant,
            tag,
            op_type,
            body,
            writer_sequence,
            previous_writer_hash,
        )
    }

    /// The tenant-op workhorse, zone/space-parametrized (epochs stay
    /// 1 — every fixture zone is at its creation epochs).
    #[allow(clippy::too_many_arguments)]
    pub fn tenant_op_in(
        &mut self,
        zone_id: Bytes16,
        space_id: Bytes16,
        actor_kind: ActorKind,
        dev: &Device,
        grant: &Grant,
        tag: &str,
        op_type: &str,
        body: cbor::Value,
        writer_sequence: u64,
        previous_writer_hash: Option<Bytes32>,
    ) -> Signedop {
        self.tenant_op_over(
            zone_id,
            space_id,
            actor_kind,
            dev,
            grant,
            tag,
            op_type,
            body,
            writer_sequence,
            previous_writer_hash,
            TenantOverrides::default(),
        )
    }

    /// [`Self::tenant_op_in`] with header overrides — the negative
    /// and epoch-currency corpus vectors mint through this seam.
    #[allow(clippy::too_many_arguments)]
    pub fn tenant_op_over(
        &mut self,
        zone_id: Bytes16,
        space_id: Bytes16,
        actor_kind: ActorKind,
        dev: &Device,
        grant: &Grant,
        tag: &str,
        op_type: &str,
        body: cbor::Value,
        writer_sequence: u64,
        previous_writer_hash: Option<Bytes32>,
        over: TenantOverrides,
    ) -> Signedop {
        let request_id = draw_id(&mut self.rng, &format!("{tag}.request_id"));
        let hlc = self.next_hlc();
        let header = Header {
            tenant: Tenant::Memory,
            plane_id: self.plane_id,
            zone_id,
            space_id,
            authored_kek_epoch: over.authored_kek_epoch,
            capability_epoch: over.capability_epoch,
            signer_alg: Sigalg::Ed25519,
            signer_key_id: suite::key_id("ed25519", &dev.sig_pk),
            writer: Writer {
                lineage: dev.lineage,
                gen: 1,
            },
            actor: Actor {
                kind: actor_kind,
                id: over.actor_id.unwrap_or_else(|| hex(&dev.device_id)),
                attested_by: None,
            },
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
            operation_type: op_type.into(),
            operation_version: 1,
            body_hash: [0; 32], // set by seal_op
        };
        let op = seal_op(header, body, &OpSigner::Ed25519(&dev.sig_sk));
        assert!(op.verify(&dev.sig_pk), "tenant op must verify");
        op
    }

    /// Seal a control op at the NEXT position under a caller-chosen
    /// `request_id` (the O5 replay negatives reuse a consumed one) —
    /// advances the chain like `seal_ctrl`.
    pub fn seal_ctrl_with_request(
        &mut self,
        op_type: &str,
        proof: Authproof,
        body: cbor::Value,
        request_id: Bytes16,
    ) -> Signedop {
        let seq = self.ctrl_seq + 1;
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

    /// A tenant `m.claim` (plain propose).
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
        self.tenant_op(
            dev,
            grant,
            tag,
            Mclaim::OP_TYPE,
            body.to_value(),
            writer_sequence,
            previous_writer_hash,
        )
    }

    /// [`Self::claim`] with header overrides (epoch-2 writers, §9.4).
    #[allow(clippy::too_many_arguments)]
    pub fn claim_over(
        &mut self,
        dev: &Device,
        grant: &Grant,
        tag: &str,
        statement: &str,
        writer_sequence: u64,
        previous_writer_hash: Option<Bytes32>,
        over: TenantOverrides,
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
        let (zone_id, space_id) = (self.zone_id, self.home_space);
        self.tenant_op_over(
            zone_id,
            space_id,
            ActorKind::Daemon,
            dev,
            grant,
            tag,
            Mclaim::OP_TYPE,
            body.to_value(),
            writer_sequence,
            previous_writer_hash,
            over,
        )
    }

    /// `c.revoke_grant` (admin arm) — cutoff REQUIRED when the grant
    /// is op-authoring (D-78/D-143).
    pub fn revoke_grant_op(
        &mut self,
        grant_id: Bytes16,
        cutoff: Option<Frontierclose>,
    ) -> Signedop {
        let body = Crevokegrant { grant_id, cutoff };
        let proof = Authproof::Admin {
            epoch: 1,
            ctrl_frontier: self.ctrl_head,
        };
        self.seal_ctrl(Crevokegrant::OP_TYPE, proof, body.to_value())
    }

    /// A tenant `m.export.release` by `dev` to a plane destination —
    /// a genuine signed triple (the journal fixtures seal it into the
    /// release ItemCommit, so `release_op = H_op(triple)` is real; its
    /// own fold admission is outside the journal machine's scope).
    #[allow(clippy::too_many_arguments)]
    pub fn release_op_signed(
        &mut self,
        dev: &Device,
        grant: &Grant,
        tag: &str,
        export_id: Bytes16,
        sources: Vec<Bytes32>,
        content_digest: Bytes32,
        dest_zone: Bytes16,
        dest_space: Bytes16,
        data_frontier: Bytes32,
        writer_sequence: u64,
        previous_writer_hash: Option<Bytes32>,
    ) -> Signedop {
        let body = Mexportrel {
            export_id,
            sources,
            content_digest,
            to: Endpoint::Plane {
                plane_id: self.plane_id,
                zone_id: dest_zone,
                space_id: dest_space,
            },
            class_floor: Class::Private,
            data_frontier,
            control_frontier: self.ctrl_head,
            as_of_ms: self.hlc_ms,
            expiry_deadline_ms: self.hlc_ms + 86_400_000,
        };
        self.tenant_op(
            dev,
            grant,
            tag,
            Mexportrel::OP_TYPE,
            body.to_value(),
            writer_sequence,
            previous_writer_hash,
        )
    }

    /// Seal `op` as an item in the genesis zone: drawn DEK + nonce,
    /// §5.3 wrap under the epoch-1 KEK — a byte-honest ItemCommit
    /// whose plaintext `(lineage, gen, seq)` equal the sealed
    /// header's BY CONSTRUCTION (I4).
    pub fn seal_commit(&mut self, op: &Signedop, tag: &str) -> Itemcommit {
        let dek = self.rng.draw32(&format!("{tag}.dek"));
        let nonce = self.rng.draw12(&format!("{tag}.nonce"));
        let core = keyschedule::seal_item(&dek, nonce, &self.plane_id, &self.zone_id, &op.encode());
        let addr = keyschedule::item_addr(&core);
        let wrapped =
            keyschedule::wrap_dek(&self.kek_e1, &self.plane_id, &self.zone_id, 1, &addr, &dek);
        Itemcommit {
            core,
            wrap: Itemwrap {
                item_addr: addr,
                key_wrap_epoch: 1,
                wrapped_dek: wrapped,
            },
            lineage: op.header.writer.lineage,
            gen: op.header.writer.gen,
            seq: op.header.writer_sequence,
        }
    }

    /// `c.zone_create` — a second zone at epoch 1 under the B.1 solo
    /// posture, with the given recipient wraps.
    pub fn zone_create(&mut self, zone_id: Bytes16, wraps: Vec<Kekwrap>) -> Signedop {
        let body = Czonecreate {
            zone_id,
            wraps,
            zone_policy: scenario::genesis_zone_policy(zone_id),
        };
        let proof = Authproof::Admin {
            epoch: 1,
            ctrl_frontier: self.ctrl_head,
        };
        self.seal_ctrl(Czonecreate::OP_TYPE, proof, body.to_value())
    }

    /// `c.space_create` — the body is a bare `spacedef`.
    pub fn space_create(&mut self, space: Spacedef) -> Signedop {
        let proof = Authproof::Admin {
            epoch: 1,
            ctrl_frontier: self.ctrl_head,
        };
        self.seal_ctrl(CSPACECREATE_OP_TYPE, proof, space.to_value())
    }

    /// The control chain's current `(seq, head op hash)` — C3′ bases
    /// name these.
    pub fn ctrl_position(&self) -> (u64, Bytes32) {
        (self.ctrl_seq, self.ctrl_head)
    }

    /// `c.recovery_succession` (C3′): recovery-arm, signed by the
    /// revealed recovery key; placement frozen at
    /// `writer_sequence = base.seq + 1`, `previous_writer_hash =
    /// base.op` (§7.4). Advances the rig chain (the fixture recovery
    /// bases on the head and cuts nothing control-side).
    /// `c.drill` — a recovery-signed nonce statement at the next
    /// chain position (recovery-arm admission; repoch = CURRENT,
    /// this is a proof, not a succession).
    pub fn drill_op(&mut self, repoch: u64) -> Signedop {
        let seq = self.ctrl_seq + 1;
        let nonce = draw_id(&mut self.rng, "drill.nonce");
        let request_id = draw_id(&mut self.rng, &format!("ctrl{seq}.request_id"));
        let hlc = self.next_hlc();
        let header = ctrl_header(
            self.plane_id,
            CTRL_ZONE,
            CTRL_SPACE,
            Sigalg::Ed25519,
            suite::key_id("ed25519", &self.recovery_pk),
            Writer {
                lineage: CTRL_LINEAGE,
                gen: 1,
            },
            Authproof::Recovery {
                repoch,
                recovery_pk: self.recovery_pk,
            },
            request_id,
            seq,
            Some(self.ctrl_head),
            hlc,
            Cdrill::OP_TYPE,
        );
        let op = seal_op(
            header,
            Cdrill { nonce }.to_value(),
            &OpSigner::Ed25519(&self.recovery_sk),
        );
        self.ctrl_seq = seq;
        self.ctrl_head = op.op_hash();
        op
    }

    pub fn recovery_op(&mut self, body: Crecovsucc) -> Signedop {
        let seq = body.base_seq + 1;
        self.recovery_op_tagged(&format!("ctrl{seq}"), body)
    }

    /// [`Self::recovery_op`] with an explicit draw tag — a below-head
    /// base reuses a position whose `ctrl{seq}` draw name is already
    /// spent.
    pub fn recovery_op_tagged(&mut self, tag: &str, body: Crecovsucc) -> Signedop {
        let seq = body.base_seq + 1;
        let request_id = draw_id(&mut self.rng, &format!("{tag}.request_id"));
        let hlc = self.next_hlc();
        let header = ctrl_header(
            self.plane_id,
            CTRL_ZONE,
            CTRL_SPACE,
            Sigalg::Ed25519,
            suite::key_id("ed25519", &self.recovery_pk),
            Writer {
                lineage: CTRL_LINEAGE,
                gen: 1,
            },
            Authproof::Recovery {
                repoch: body.repoch,
                recovery_pk: self.recovery_pk,
            },
            request_id,
            seq,
            Some(body.base_op),
            hlc,
            Crecovsucc::OP_TYPE,
        );
        let op = seal_op(
            header,
            body.to_value(),
            &OpSigner::Ed25519(&self.recovery_sk),
        );
        assert!(op.verify(&self.recovery_pk), "recovery op must verify");
        self.ctrl_seq = seq;
        self.ctrl_head = op.op_hash();
        op
    }

    /// A signed storage receipt from `dev` (issuer_seq 1) — a real
    /// `Signedstmt` whose `stmt_id` fixtures can cite.
    pub fn storage_receipt(&mut self, dev: &Device, subject: Bytes32) -> Signedstmt {
        let stmt = Receiptstmt::Storage {
            issuer: Issuerid::Device {
                cert: h_cert(&dev.cert),
            },
            plane_id: self.plane_id,
            zone_id: self.zone_id,
            subject,
            size: 512,
            seen_ms: self.hlc_ms,
            issuer_seq: 1,
            prev_stmt: [0; 32],
        };
        sign_receipt(&stmt, &OpSigner::Ed25519(&dev.sig_sk))
    }
}

/// Canonical bytes of any shape — journal frames enter the items map
/// this way.
fn enc(v: &impl ToValue) -> Vec<u8> {
    cbor::encode(&v.to_value()).expect("shape encodes")
}

pub(crate) fn items_raw(entries: &[(&str, &[u8])]) -> Json {
    let mut m = JsonMap::new();
    for (name, b) in entries {
        m.insert((*name).into(), json!(hex(b)));
    }
    Json::Object(m)
}

pub(crate) fn items(entries: &[(&str, &Signedop)]) -> Json {
    let mut m = JsonMap::new();
    for (name, op) in entries {
        m.insert((*name).into(), json!(hex(&op.encode())));
    }
    Json::Object(m)
}

pub(crate) fn admits(item: &str) -> Json {
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

/// The shared journal preamble both journal fixtures mint: a real
/// release triple sealed into its ItemCommit, committed in ONE Txn
/// with the PendingXfer (§6.2 — the source-zone commit shape), plus
/// the revocation op the abort bases cite (held via `aux`).
struct JournalPreamble {
    rig: PlaneRig,
    export_id: Bytes16,
    release_op: Bytes32,
    srcs: Vec<Bytes32>,
    t1: Txn,
    revoke_op: Signedop,
    grant3_op: Signedop,
}

fn journal_preamble(name: &str, source_count: usize) -> JournalPreamble {
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();

    // The op the abort bases cite: a real revocation of a real import
    // grant (issued to a second device, then revoked — the
    // revoked-grant `no-grant` cause of the missing imports).
    let dev2 = rig.mint_device("dev2");
    let g3 = rig.simple_grant("grant3", &dev2, vec![Verb::Import]);
    let g3_id = g3.grant_id;
    let grant3_op = rig.grant_op(g3);
    let revoke_op = rig.revoke_grant_op(
        g3_id,
        // import is op-authoring: the revocation carries the target
        // lineage's (empty-heads) authorship cutoff (D-143).
        Some(Frontierclose {
            zone_id: rig.zone_id,
            lineage: dev2.lineage,
            heads: vec![],
        }),
    );

    // The release: real signed triple, sealed byte-honestly into the
    // journal's opening Txn. Source ids are opaque to the journal
    // machine (drawn; never dereferenced by replay).
    let export_id = draw_id(&mut rig.rng, "rel.export_id");
    let srcs: Vec<Bytes32> = (1..=source_count)
        .map(|i| rig.rng.draw32(&format!("src{i}.op_hash")))
        .collect();
    let content_digest = rig.rng.draw32("rel.content_digest");
    let dest_zone = draw_id(&mut rig.rng, "dest.zone_id");
    let dest_space = draw_id(&mut rig.rng, "dest.space_id");
    let data_frontier = rig.rng.draw32("rel.data_frontier");
    let grant = rig.genesis_grant.clone();
    let rel = rig.release_op_signed(
        &d1,
        &grant,
        "rel",
        export_id,
        srcs.clone(),
        content_digest,
        dest_zone,
        dest_space,
        data_frontier,
        1,
        None,
    );
    let release_op = rel.op_hash();
    let commit = rig.seal_commit(&rel, "rel.item");
    let t1 = Txn {
        records: vec![
            Txnrec::ItemCommit(commit),
            Txnrec::PendingXfer(Pendingxfer {
                export_id,
                release_op,
                dest_zone,
                content_digest,
                record_count: source_count as u64,
            }),
        ],
    };

    JournalPreamble {
        rig,
        export_id,
        release_op,
        srcs,
        t1,
        revoke_op,
        grant3_op,
    }
}

/// Tranche #4 — family 13 journal-replay: Abort/Reopen inside one
/// Txn, then competing terminals inside one Txn (D-200). Journal
/// order is `(frame ordinal, record index)`: t2's abort terminals
/// interval 0 and its reopen — validating SEQUENTIALLY against
/// transaction-local state — opens interval 1 (frame order alone
/// could never order the pair). t3 then carries a Done AND an Abort
/// for interval 1: the second terminal in one interval is a journal
/// invariant violation, `(log-corrupt, storage-quarantine)`, and the
/// commit is all-or-nothing — the Done is DISCARDED with it, so
/// interval 1 stays open (the intervals result proves the discard).
pub fn f13_txn_internal_order() -> Vector {
    let name = "txn-internal-order-and-competing-terminals";
    let mut p = journal_preamble(name, 2);
    let (eid, rop) = (p.export_id, p.release_op);
    let x = Opfactref(p.revoke_op.op_hash());
    let (r1, r2) = (p.srcs[0], p.srcs[1]);

    // The reopen's invalidation: a real signed statement, HELD via
    // aux (the journal machine checks holding and basis-match; cause
    // sufficiency is fold territory).
    let d1 = p.rig.dev1.clone();
    let fork_stmt = p.rig.storage_receipt(&d1, rop);
    let s_id = fork_stmt.stmt_id();

    let t2 = Txn {
        records: vec![
            Txnrec::XferAbort(Xferabort {
                export_id: eid,
                release_op: rop,
                reason: AbortReason::RejectPermanent,
                incarnation: 0,
                missing: vec![
                    MissingRec {
                        rec: r1,
                        basis: Some(x),
                    },
                    MissingRec {
                        rec: r2,
                        basis: Some(x),
                    },
                ],
            }),
            Txnrec::XferReopen(Xferreopen {
                export_id: eid,
                release_op: rop,
                incarnation: 0,
                basis: x,
                invalidation: Factref::Stmt(s_id),
            }),
        ],
    };
    let t3 = Txn {
        records: vec![
            Txnrec::XferDone(Xferdone {
                export_id: eid,
                release_op: rop,
                incarnation: 1,
                completed: vec![r1, r2],
            }),
            Txnrec::XferAbort(Xferabort {
                export_id: eid,
                release_op: rop,
                reason: AbortReason::RejectPermanent,
                incarnation: 1,
                missing: vec![MissingRec {
                    rec: r2,
                    basis: Some(x),
                }],
            }),
        ],
    };

    let mut inputs = JsonMap::new();
    inputs.insert(
        "items".into(),
        items_raw(&[("t1", &enc(&p.t1)), ("t2", &enc(&t2)), ("t3", &enc(&t3))]),
    );
    inputs.insert("deliveries".into(), json!([["t1", "t2", "t3"]]));
    let mut aux = JsonMap::new();
    aux.insert("grant3.op".into(), json!(hex(&p.grant3_op.encode())));
    aux.insert("revoke.op".into(), json!(hex(&p.revoke_op.encode())));
    aux.insert("fork.stmt".into(), json!(hex(&enc(&fork_stmt))));
    inputs.insert("aux".into(), Json::Object(aux));

    Vector {
        family: 13,
        name: name.into(),
        case_kind: "journal-replay".into(),
        source: "6.2".into(),
        surfaces: vec![
            "browser".into(),
            "storage-macos".into(),
            "storage-linux".into(),
            "storage-windows".into(),
        ],
        rng: Some(p.rig.rng.into_json()),
        inputs,
        expected: Expected::Result(json!({
            "intervals": [
                { "incarnation": 0, "terminal": "abort" },
                { "incarnation": 1, "terminal": "open" },
            ],
            "per_record": [
                { "rec": "t1" },
                { "rec": "t2" },
                { "rec": "t3", "outcome": "log-corrupt", "disposition": "storage-quarantine" },
            ],
            "converge": true,
        })),
    }
}

/// Tranche #3 — family 11 journal-replay: the D-193/D-200 basis
/// typing. t3 is a well-formed reopen whose op-kind basis is held
/// but whose stmt-kind INVALIDATION cites an unheld statement — it
/// verifies as a shape and PENDS `ref-unresolved` (verifiable-when-
/// held, D-163/D-185), reserving the interval. t4 carries a
/// stmt-kind BASIS — structurally outside `opfactref`, so it fails
/// at parse: `(log-corrupt, storage-quarantine)`; a parse-invalid
/// record classifies immediately (it is not a valid transition, so
/// the reservation queue never sees it).
pub fn f11_reopen_basis_types() -> Vector {
    let name = "reopen-basis-op-kind-and-unheld-invalidation";
    let mut p = journal_preamble(name, 1);
    let (eid, rop) = (p.export_id, p.release_op);
    let x = Opfactref(p.revoke_op.op_hash());
    let r1 = p.srcs[0];
    let unheld_stmt = p.rig.rng.draw32("unheld.stmt_id");

    let t2 = Txn {
        records: vec![Txnrec::XferAbort(Xferabort {
            export_id: eid,
            release_op: rop,
            reason: AbortReason::RejectPermanent,
            incarnation: 0,
            missing: vec![MissingRec {
                rec: r1,
                basis: Some(x),
            }],
        })],
    };
    let t3 = Txn {
        records: vec![Txnrec::XferReopen(Xferreopen {
            export_id: eid,
            release_op: rop,
            incarnation: 0,
            basis: x,
            invalidation: Factref::Stmt(unheld_stmt),
        })],
    };
    // t4: the D-193/D-200 negative — hand-built below the typed
    // layer, which cannot express a stmt-kind basis.
    let t4_bytes = {
        use crate::shapes::{bytes, text, u};
        let bad_reopen = cbor::map(vec![
            ("export_id", bytes(&eid)),
            ("release_op", bytes(&rop)),
            ("incarnation", u(0)),
            (
                "basis",
                cbor::map(vec![("kind", text("stmt")), ("ref", bytes(&x.0))]),
            ),
            (
                "invalidation",
                Factref::Op(p.revoke_op.op_hash()).to_value(),
            ),
        ]);
        cbor::encode(&cbor::map(vec![(
            "records",
            cbor::Value::Array(vec![bad_reopen]),
        )]))
        .expect("bad txn encodes")
    };

    let mut inputs = JsonMap::new();
    inputs.insert(
        "items".into(),
        items_raw(&[
            ("t1", &enc(&p.t1)),
            ("t2", &enc(&t2)),
            ("t3", &enc(&t3)),
            ("t4", &t4_bytes),
        ]),
    );
    inputs.insert("deliveries".into(), json!([["t1", "t2", "t3", "t4"]]));
    let mut aux = JsonMap::new();
    aux.insert("grant3.op".into(), json!(hex(&p.grant3_op.encode())));
    aux.insert("revoke.op".into(), json!(hex(&p.revoke_op.encode())));
    inputs.insert("aux".into(), Json::Object(aux));

    Vector {
        family: 11,
        name: name.into(),
        case_kind: "journal-replay".into(),
        source: "6.2".into(),
        surfaces: vec!["core".into()],
        rng: Some(p.rig.rng.into_json()),
        inputs,
        expected: Expected::Result(json!({
            "intervals": [
                { "incarnation": 0, "terminal": "abort" },
            ],
            "per_record": [
                { "rec": "t1" },
                { "rec": "t2" },
                { "rec": "t3", "outcome": "ref-unresolved", "disposition": "pending-dependency" },
                { "rec": "t4", "outcome": "log-corrupt", "disposition": "storage-quarantine" },
            ],
            "converge": true,
        })),
    }
}

/// Tranche #6 — family 7 fold: the staged frontier is consumed by the
/// authority-ending revocation and never resurrects under regrant
/// (D-153/D-196). `s` stages dev2's frontier close (inert). `rg`
/// revokes dev2's LAST op-authoring grant — the authority-ending
/// frontier vacuously consumes the stage at its acceptance,
/// materializing nothing. `g4` regrants dev2 (the lineage re-enters
/// the coverage domain as NEW authority). `k1`, an epoch advance
/// covering only dev1's lineage — counting on the dead stage for
/// dev2 — REJECTS (`body-invariant`: strict-zone union coverage
/// short); `k2` with fresh total coverage ADMITS at the same chain
/// position (a failed operation exerts no precedence — D-112 — so
/// no C2 fires).
pub fn f7_staged_frontier_consumed() -> Vector {
    let name = "staged-frontier-consumed-no-resurrection";
    let mut rig = PlaneRig::new(name);

    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let grant2_id = grant2.grant_id;
    let c2 = rig.enroll_new(&dev2, vec![grant2], "wrap.dev2.eph");

    let fc_dev2 = Frontierclose {
        zone_id: rig.zone_id,
        lineage: dev2.lineage,
        heads: vec![],
    };
    let fc_dev1 = Frontierclose {
        zone_id: rig.zone_id,
        lineage: rig.dev1.lineage,
        heads: vec![],
    };

    // s: the pure staging ceremony for dev2's lineage.
    let s = rig.stage_closes(vec![fc_dev2.clone()]);

    // rg: revoke dev2's last op-authoring grant — the authority-ending
    // frontier; the stage is vacuously consumed here (D-196).
    let rg = rig.revoke_grant_op(grant2_id, Some(fc_dev2.clone()));

    // g4: regrant — new authority, fresh coverage obligations.
    let grant4 = rig.simple_grant("grant4", &dev2, vec![Verb::Propose]);
    let g4 = rig.grant_op(grant4);

    // k1: coverage for dev1 only — the dead stage must NOT count.
    let k1 = rig.epoch_bump_candidate("k1", 2, vec![fc_dev1.clone()]);
    // k2: fresh total coverage at the same position — admits.
    let k2 = rig.epoch_bump(2, vec![fc_dev1, fc_dev2]);

    let c1 = &rig.genesis_op;
    let mut inputs = JsonMap::new();
    inputs.insert(
        "items".into(),
        items(&[
            ("c1", c1),
            ("c2", &c2),
            ("s", &s),
            ("rg", &rg),
            ("g4", &g4),
            ("k1", &k1),
            ("k2", &k2),
        ]),
    );
    inputs.insert(
        "deliveries".into(),
        json!([["c1", "c2", "s", "rg", "g4", "k1", "k2"]]),
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
                admits("s"),
                admits("rg"),
                admits("g4"),
                { "item": "k1", "outcome": "body-invariant", "disposition": "reject-permanent" },
                admits("k2"),
            ],
            "converge": true,
        })),
    }
}

/// Tranche #1 — family 11 journal-replay: the D-198 deferral
/// schedule. A claim is committed, released (the source-zone Txn
/// opens a transfer journal — NONTERMINAL), and then erased by a
/// direct-human `m.erase_request`. Retrieval exclusion is IMMEDIATE
/// at the erase acceptance; the erase-queue entry exists but is
/// manifest-INELIGIBLE while any referencing journal is nonterminal —
/// the state probes pin all three facts, and the open interval pins
/// the journal. (The adopted-erasure residual — `source-erased` via a
/// C3′-adopted manifest — is a recovery-ceremony vector for the
/// corpus phase.) The release cites `gf`, dev1's flow-carrying
/// export grant — the genesis grant pins no flows (D-76), so a
/// release under it can never satisfy §11.8's flow-match; the first
/// mint did exactly that and the differential reducer caught it
/// (`no-flow` vs the expected admit).
pub fn f11_erase_deferral() -> Vector {
    let name = "erase-deferral-nonterminal-journal";
    const STMT: &str = "harbor crane inspection completed without findings";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let g1 = rig.genesis_grant.clone();
    let (gz, home) = (rig.zone_id, rig.home_space);

    let dev2 = rig.mint_device("dev2");
    let grant2 = rig.simple_grant("grant2", &dev2, vec![Verb::Propose]);
    let c2 = rig.enroll_new(&dev2, vec![grant2.clone()], "wrap.dev2.eph");

    // dev1's export grant carrying the flow to the (opaque)
    // destination endpoint.
    let dest_zone = draw_id(&mut rig.rng, "dest.zone_id");
    let dest_space = draw_id(&mut rig.rng, "dest.space_id");
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
            zone_id: dest_zone,
            space_id: dest_space,
        },
        kinds: None,
        class_ceiling: Class::Sensitive,
        expiry_deadline_ms: T0_MS + 10 * 86_400_000,
    }]);
    let gf = rig.grant_op(gf_grant.clone());

    // The item: dev2's claim, committed to the zone log.
    let i1 = rig.claim(&dev2, &grant2, "i1", STMT, 1, None);
    let i1_commit = rig.seal_commit(&i1, "i1.item");
    let i1_addr = i1_commit.wrap.item_addr;
    let t_i1 = Txn {
        records: vec![Txnrec::ItemCommit(i1_commit)],
    };

    // The release: real bundle digest (single-leaf Merkle root over
    // the exact bundlerec the destination would verify).
    let export_id = draw_id(&mut rig.rng, "rel.export_id");
    let digest = merkle_root(&[Bundleleaf {
        export_id,
        rec_index: 0,
        rec: Bundlerec {
            op: i1.op_hash(),
            kind: Kind::Observation,
            statement: STMT.into(),
            class_floor: Class::Private,
        },
    }
    .leaf_hash()]);
    let data_frontier = rig.rng.draw32("rel.data_frontier");
    let rel = rig.release_op_signed(
        &d1,
        &gf_grant,
        "rel",
        export_id,
        vec![i1.op_hash()],
        digest,
        dest_zone,
        dest_space,
        data_frontier,
        1,
        None,
    );
    let rel_commit = rig.seal_commit(&rel, "rel.item");
    let t_rel = Txn {
        records: vec![
            Txnrec::ItemCommit(rel_commit),
            Txnrec::PendingXfer(Pendingxfer {
                export_id,
                release_op: rel.op_hash(),
                dest_zone,
                content_digest: digest,
                record_count: 1,
            }),
        ],
    };

    // The erase request: direct-human evidence (shape 1), targeting
    // the claim's op hash (D-66).
    let e = rig.tenant_op_as(
        ActorKind::Human,
        &d1,
        &g1,
        "e",
        Merasereq::OP_TYPE,
        Merasereq {
            targets: vec![i1.op_hash()],
        }
        .to_value(),
        2,
        Some(rel.op_hash()),
    );

    let c1 = rig.genesis_op.clone();
    let mut inputs = JsonMap::new();
    inputs.insert(
        "items".into(),
        items_raw(&[
            ("c1", &c1.encode()),
            ("c2", &c2.encode()),
            ("gf", &gf.encode()),
            ("i1", &i1.encode()),
            ("t.i1", &enc(&t_i1)),
            ("rel", &rel.encode()),
            ("t.rel", &enc(&t_rel)),
            ("e", &e.encode()),
        ]),
    );
    inputs.insert(
        "deliveries".into(),
        json!([["c1", "c2", "gf", "i1", "t.i1", "rel", "t.rel", "e"]]),
    );

    let probe = |ids: &[Bytes32]| -> String {
        use crate::shapes::bytes;
        hex(
            &cbor::encode(&cbor::Value::Array(ids.iter().map(|i| bytes(i)).collect()))
                .expect("probe encodes"),
        )
    };

    Vector {
        family: 11,
        name: name.into(),
        case_kind: "journal-replay".into(),
        source: "5.4".into(),
        surfaces: vec!["core".into()],
        rng: Some(rig.rng.into_json()),
        inputs,
        expected: Expected::Result(json!({
            "intervals": [
                { "incarnation": 0, "terminal": "open" },
            ],
            "per_record": [
                { "rec": "c1" },
                { "rec": "c2" },
                { "rec": "gf" },
                { "rec": "i1" },
                { "rec": "t.i1" },
                { "rec": "rel" },
                { "rec": "t.rel" },
                { "rec": "e" },
            ],
            "converge": true,
            "state_probes": [
                {
                    "name": "erase-queue accepted entries, item_addrs (§5.4)",
                    "value": probe(&[i1_addr]),
                },
                {
                    "name": "manifest-eligible erase-queue entries, item_addrs (§5.4 D-198 — a nonterminal referencing journal defers)",
                    "value": probe(&[]),
                },
                {
                    "name": "retrieval-excluded claims, op hashes (§11.1 m.erase_request — immediate on acceptance)",
                    "value": probe(&[i1.op_hash()]),
                },
            ],
        })),
    }
}

/// Tranche #7 — family 11 fold: the collision loser re-enters when
/// the frozen winner dies (D-155/D-161/D-169/D-196; the §13.3
/// family-11 arc "A-frozen → B-collision → A dies → B re-enters and
/// may own"). dev2's import `m1` admits and is FROZEN by the
/// revocation of its grant with a cutoff preserving it (the frontier
/// forecloses its lineage's claim room, D-155). dev3 — the next
/// import grant, so the next claimant in the (grant position, gen,
/// seq) total order — imports the same released record byte-distinct:
/// a claim against a frozen owner, `import-collision`
/// (quarantine-reproposal, the DERIVED lane). The C3′ recovery bases
/// on the chain head (nothing control-side cut), names dev1's and
/// dev3's tenant histories, and OMITS (Z2, dev2.lineage): the
/// revivable recovery-omission blanket quarantines the winner
/// (`cutoff`, D-140). The total re-fold re-derives the claimant fold:
/// the first SURVIVING claimant is now `m2` — the former loser owns
/// provisionally (m1's revivable quarantine reserves the key against
/// freezing, D-161/D-169) and ADMITS.
pub fn f11_collision_loser_reentry() -> Vector {
    let name = "collision-loser-reenters-on-winner-death";
    const STMT: &str = "quarterly reconciliation ledger balanced to zero";
    let mut rig = PlaneRig::new(name);
    let d1 = rig.dev1.clone();
    let g1 = rig.genesis_grant.clone();
    let (gz, home) = (rig.zone_id, rig.home_space);

    // Z2: the destination zone (epoch 1, wrapped to dev1).
    let z2 = draw_id(&mut rig.rng, "zone2.zone_id");
    let kek2 = rig.rng.draw32("kek.zone2.e1");
    let w1 = rig.wrap_in(z2, &kek2, d1.device_id, &d1.kem_pk, 1, "wrap.z2.dev1.eph");
    let cz = rig.zone_create(z2, vec![w1]);

    // Z2's project space (workflow-v1 status policy).
    let z2_space = draw_id(&mut rig.rng, "zone2.space_id");
    let z2_name_hash = rig.rng.draw32("zone2.space.name_hash");
    let cs = rig.space_create(Spacedef {
        space_id: z2_space,
        zone_id: z2,
        name_hash: z2_name_hash,
        space_class: Spaceclass::Project,
        class_minimum: Class::Private,
        status_policy: Polref {
            id: "workflow-v1".into(),
            version: 1,
            hash: scenario::workflow_v1().hash(),
        },
    });

    // dev2 + the zone's FIRST import grant (one active per zone).
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

    // dev1's export grant carrying the flow to Z2 (the genesis grant
    // pins no flows — D-76 — so the release needs this one).
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

    // The source claim and its release (REAL stamp: the genesis
    // zone's frontier hash at i1, and this fixture's real bundle).
    let i1 = rig.claim(&d1, &g1, "i1", STMT, 1, None);
    let export_id = draw_id(&mut rig.rng, "rel.export_id");
    let digest = merkle_root(&[Bundleleaf {
        export_id,
        rec_index: 0,
        rec: Bundlerec {
            op: i1.op_hash(),
            kind: Kind::Observation,
            statement: STMT.into(),
            class_floor: Class::Private,
        },
    }
    .leaf_hash()]);
    let stamp = Frontier {
        zone_id: gz,
        heads: vec![Head {
            lineage: d1.lineage,
            gen: 1,
            seq: 1,
            op: i1.op_hash(),
        }],
    }
    .hash();
    let rel = rig.release_op_signed(
        &d1,
        &gf_grant,
        "rel",
        export_id,
        vec![i1.op_hash()],
        digest,
        z2,
        z2_space,
        stamp,
        2,
        Some(i1.op_hash()),
    );

    // m1: dev2's import — the D-134 fully-derived content; single
    // leaf, so the Merkle path is empty and the leaf IS the root.
    let import_body = Mimport {
        source_op: i1.op_hash(),
        class_floor: Class::Private,
        kind: Kind::Observation,
        statement: STMT.into(),
        sensitivity: Class::Private,
        rec_index: 0,
        proof: vec![],
        from_plane: rig.plane_id,
        export_id,
        release_op: rel.op_hash(),
        digest,
    };
    let m1 = rig.tenant_op_in(
        z2,
        z2_space,
        ActorKind::Daemon,
        &dev2,
        &g2,
        "m1",
        Mimport::OP_TYPE,
        import_body.to_value(),
        1,
        None,
    );

    // rg: revoke g2 with the cutoff AT m1's head — m1 is preserved
    // at-or-below and frozen by the frontier that forecloses its
    // lineage's remaining claim room (D-155).
    let rg = rig.revoke_grant_op(
        g2.grant_id,
        Some(Frontierclose {
            zone_id: z2,
            lineage: dev2.lineage,
            heads: vec![Head {
                lineage: dev2.lineage,
                gen: 1,
                seq: 1,
                op: m1.op_hash(),
            }],
        }),
    );

    // dev3 + the successor import grant (legal: g2 is revoked).
    let dev3 = rig.mint_device("dev3");
    let g3 = rig.grant_in("grant3", &dev3, vec![Verb::Import], z2, vec![z2_space]);
    let w3 = rig.wrap_in(
        z2,
        &kek2,
        dev3.device_id,
        &dev3.kem_pk,
        1,
        "wrap.z2.dev3.eph",
    );
    let c3 = rig.enroll_new_with_wraps(&dev3, vec![g3.clone()], vec![w3]);

    // m2: the same record, byte-distinct (new writer, new request) —
    // a claim against the FROZEN m1.
    let m2 = rig.tenant_op_in(
        z2,
        z2_space,
        ActorKind::Daemon,
        &dev3,
        &g3,
        "m2",
        Mimport::OP_TYPE,
        import_body.to_value(),
        1,
        None,
    );

    // r: the C3′ — base = the chain head (nothing control-side cut);
    // dev1's and dev3's tenant histories NAMED at their heads;
    // (Z2, dev2.lineage) OMITTED — the revivable blanket kills the
    // frozen winner.
    let (base_seq, base_op) = rig.ctrl_position();
    let admin2_seed = rig.rng.draw32("admin2.sig_seed");
    let (_a2_sk, a2_pk) = suite::ed25519::keypair(&admin2_seed);
    let recovery2_seed = rig.rng.draw32("recovery2.sig_seed");
    let (_r2_sk, r2_pk) = suite::ed25519::keypair(&recovery2_seed);
    let r = rig.recovery_op(Crecovsucc {
        base_seq,
        base_op,
        epoch: 2,
        repoch: 1,
        new_admin: AdminKey {
            alg: Sigalg::Ed25519,
            pk: a2_pk.to_vec(),
        },
        new_recovery_commitment: h_tag(Tag::Drill, &r2_pk),
        tenant_cutoffs: vec![
            Frontierclose {
                zone_id: gz,
                lineage: d1.lineage,
                heads: vec![Head {
                    lineage: d1.lineage,
                    gen: 1,
                    seq: 2,
                    op: rel.op_hash(),
                }],
            },
            Frontierclose {
                zone_id: z2,
                lineage: dev3.lineage,
                heads: vec![Head {
                    lineage: dev3.lineage,
                    gen: 1,
                    seq: 1,
                    op: m2.op_hash(),
                }],
            },
        ],
        adopted_renewals: None,
        retired_keys: None,
        adopted_rotations: vec![],
    });

    let c1 = &rig.genesis_op;
    let mut inputs = JsonMap::new();
    inputs.insert(
        "items".into(),
        items(&[
            ("c1", c1),
            ("cz", &cz),
            ("cs", &cs),
            ("c2", &c2),
            ("gf", &gf),
            ("i1", &i1),
            ("rel", &rel),
            ("m1", &m1),
            ("rg", &rg),
            ("c3", &c3),
            ("m2", &m2),
            ("r", &r),
        ]),
    );
    inputs.insert(
        "deliveries".into(),
        json!([["c1", "cz", "cs", "c2", "gf", "i1", "rel", "m1", "rg", "c3", "m2", "r"]]),
    );

    Vector {
        family: 11,
        name: name.into(),
        case_kind: "fold".into(),
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
                admits("rel"),
                { "item": "m1", "outcome": "cutoff", "disposition": "quarantine-reproposal" },
                admits("rg"),
                admits("c3"),
                admits("m2"),
                admits("r"),
            ],
            "converge": true,
            "trace": [{
                "delivery": 0,
                "after": "m2",
                "item": "m2",
                "outcome": "import-collision",
                "disposition": "quarantine-reproposal",
            }],
        })),
    }
}

/// Every tranche fixture, in annex order (grows as builders land).
pub fn tranche() -> Vec<Vector> {
    vec![
        f7_delayed_reference_convergence(),
        f7_negation_residual(),
        f7_pending_revocation_window_grant(),
        f7_staged_frontier_consumed(),
        f11_collision_loser_reentry(),
        f11_erase_deferral(),
        f11_reopen_basis_types(),
        f13_txn_internal_order(),
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
        // D-155/D-161/D-196 — the collision-loser arc (family 11).
        "a claim
against a frozen owner is `import-collision` — a fold outcome in
the DERIVED lane,
never a terminal cause (D-177/D-196) — and the freeze basis dying
unfreezes the key at the FOLD level: the claimant fold re-derives
INCLUDING the former collision loser (A-frozen → B-collision →
A's proof dies → B re-enters and may own",
        "(an at-or-below
preserved claimant after grant revocation is thereby CLASSIFIED:
frozen by the frontier that forecloses its competitors, D-155)",
        "a claimant pending
proof AND a claimant in revivable quarantine both RESERVE the key
against freezing",
        "beyond a ratify boundary or the recovery-omission blanket = revivable",
        "effective owner at any fold position is the order's first
surviving claimant; ownership **freezes** when a derived predicate",
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

    /// #7 internals: the two imports share the replay key but are
    /// byte-distinct; the single-leaf Merkle proof folds to the
    /// release digest; the C3′ placement arithmetic holds and the
    /// recovery commitment matches the revealed key.
    #[test]
    fn f11_collision_internals() {
        use crate::shapes::memory::fold_proof;
        let v = f11_collision_loser_reentry();
        let items = &v.to_json()["inputs"]["items"];
        let unhex = |s: &str| -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        };
        let m1 = unhex(items["m1"].as_str().unwrap());
        let m2 = unhex(items["m2"].as_str().unwrap());
        assert_ne!(m1, m2, "claimants must be byte-distinct");

        // Rebuild the rig deterministically for structured access.
        let name = "collision-loser-reenters-on-winner-death";
        let mut rig = PlaneRig::new(name);
        let d1 = rig.dev1.clone();
        let z2 = draw_id(&mut rig.rng, "zone2.zone_id");
        let kek2 = rig.rng.draw32("kek.zone2.e1");
        let w1 = rig.wrap_in(z2, &kek2, d1.device_id, &d1.kem_pk, 1, "wrap.z2.dev1.eph");
        // The Z2 wrap really opens to dev1 under Z2's context.
        assert_eq!(
            keyschedule::open_kek(&d1.kem_sk, &rig.plane_id, &z2, 1, &w1.enc, &w1.ct),
            Some(kek2)
        );

        // The import's empty proof folds the single leaf to the root.
        let g1 = rig.genesis_grant.clone();
        let _cz = rig.zone_create(z2, vec![w1]);
        let z2_space = draw_id(&mut rig.rng, "zone2.space_id");
        let _nh = rig.rng.draw32("zone2.space.name_hash");
        let dev2 = rig.mint_device("dev2");
        let _g2 = rig.grant_in("grant2", &dev2, vec![Verb::Import], z2, vec![z2_space]);
        let _w2 = rig.wrap_in(
            z2,
            &kek2,
            dev2.device_id,
            &dev2.kem_pk,
            1,
            "wrap.z2.dev2.eph",
        );
        let _gfid = draw_id(&mut rig.rng, "grantflow.grant_id");
        let i1 = rig.claim(
            &d1,
            &g1,
            "i1.probe",
            "quarterly reconciliation ledger balanced to zero",
            1,
            None,
        );
        let export_id = draw_id(&mut rig.rng, "rel.export_id.probe");
        let leaf = Bundleleaf {
            export_id,
            rec_index: 0,
            rec: Bundlerec {
                op: i1.op_hash(),
                kind: Kind::Observation,
                statement: "quarterly reconciliation ledger balanced to zero".into(),
                class_floor: Class::Private,
            },
        }
        .leaf_hash();
        let digest = merkle_root(&[leaf]);
        assert_eq!(fold_proof(leaf, 0, 1, &[]), Some(digest));
    }
}
