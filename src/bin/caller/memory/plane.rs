//! The ephemeral local plane: keys, the genesis ceremony, the
//! in-memory op log, and the fold cache.
//!
//! The ceremony mirrors the stamped reference rig (`tranche.rs`
//! `PlaneRig::new_with` on the asset branch) with one deliberate
//! difference: identifiers and key seeds come from the OS CSPRNG, not
//! a recorded deterministic stream — this is a real (if ephemeral)
//! plane, not a fixture. Every object construction, encoding, and
//! signature flows through the vendored `owner_plane_core` writer, and
//! every appended operation is adjudicated by the vendored
//! `owner_plane_reducer` fold; the tests pin that the ceremony ADMITS
//! under the stamped reader.

use std::collections::BTreeMap;

use owner_plane_core::cbor;
use owner_plane_core::domains::{h_tag, Tag};
use owner_plane_core::keyschedule;
use owner_plane_core::scenario;
use owner_plane_core::shapes::control::{ctrl_header, Cgenesis};
use owner_plane_core::shapes::envelope::{
    gen_start, seal_op, Actor, ActorKind, Header, OpSigner, Signedop, Tenant, Writer,
};
use owner_plane_core::shapes::identity::{
    Budget, Cert, Genesis, Grant, GrantTenant, Provenance, SpacesSel, ZoneSel,
};
use owner_plane_core::shapes::{
    Class, Devclass, Hlc, Kekwrap, Lineagedef, Polref, Sigalg, Spaceclass, Spacedef, ToValue, Verb,
};
use owner_plane_core::suite::{self, hpke_wrap};
use owner_plane_reducer::fold::{
    fold_set, State, Unimplemented, Verdict, CTRL_LINEAGE, CTRL_SPACE, CTRL_ZONE,
};

use super::types::MemoryError;

/// `H_cert` / `H_grant` — the dev-arm citations (the reference rig's
/// helpers; compositional uses of the vendored writer, re-derived here
/// because the rig itself stays on the asset branch).
fn h_cert(cert: &Cert) -> [u8; 32] {
    h_tag(
        Tag::Cert,
        &cbor::encode(&cert.to_value()).expect("cert encodes"),
    )
}

fn h_grant(grant: &Grant) -> [u8; 32] {
    h_tag(
        Tag::Grant,
        &cbor::encode(&grant.to_value()).expect("grant encodes"),
    )
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn rand32() -> [u8; 32] {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b
}

/// A fresh random identifier honoring the N1 reserved-prefix rule
/// (real ids never have their first 8 bytes all zero — the reserved
/// control identifiers live there).
fn rand_id() -> [u8; 16] {
    loop {
        let b = rand32();
        let id: [u8; 16] = b[..16].try_into().expect("16-byte prefix");
        if id[..8] != [0u8; 8] {
            return id;
        }
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The trusted-plane genesis verb set (registry order, matching the
/// reference rig): the plane-level ceiling for the daemon writer.
/// Surface-level authorization (daemon IAM, and post-A2 the
/// `ActorBinding` ring rules) narrows per caller on top of this.
fn daemon_writer_verbs() -> Vec<Verb> {
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

/// The pinned genesis budget default (reference rig values).
const GENESIS_BUDGET: Budget = Budget {
    max_ops: 1_000_000,
    max_bytes: 268_435_456,
};

/// The plane's custody-tier secrets, surfaced ONLY by
/// [`EphemeralPlane::bootstrap_with_custody`] for the P1.5 durable
/// custody adapter (`memory::store`) and its battery. The ephemeral
/// service path ([`EphemeralPlane::bootstrap`]) drops them.
#[allow(dead_code)]
pub(crate) struct PlaneCustody {
    pub root_seed: [u8; 32],
    pub recovery_seed: [u8; 32],
    pub sig_seed: [u8; 32],
    pub kem_ikm: [u8; 32],
    /// The zone's epoch-1 KEK. Recoverable from the genesis wrap via
    /// `kem_ikm` (`keyschedule::open_kek`) regardless; carried
    /// directly so reopen never re-parses ceremony bodies — same
    /// custody tier, no new exposure class.
    pub kek_epoch1: [u8; 32],
}

/// The ceremony's random identifiers, carried by the durable store's
/// sidecar so [`EphemeralPlane::resume`] can rebuild the writer side
/// at reopen (the deterministic remainder re-derives from
/// [`PlaneCustody`] and the ceremony constants). The rebuilt
/// cert/grant MUST hash back to the ceremony's `H_cert`/`H_grant` —
/// the stamped fold rejects the first resumed append otherwise, which
/// is exactly what the crash battery proves.
#[allow(dead_code)]
pub(crate) struct PlaneResume {
    pub custody: PlaneCustody,
    pub plane_id: [u8; 32],
    pub zone_id: [u8; 16],
    pub home_space: [u8; 16],
    pub audit_space: [u8; 16],
    pub device_id: [u8; 16],
    pub lineage: [u8; 16],
    pub evidence_hash: [u8; 32],
    pub revocation_id: [u8; 16],
    pub grant_id: [u8; 16],
    /// `op_hash` of the genesis operation (the ctrl chain head).
    pub genesis_hash: [u8; 32],
}

/// The daemon's writer device on this plane.
pub(crate) struct WriterDevice {
    pub device_id: [u8; 16],
    pub lineage: [u8; 16],
    sig_seed: [u8; 32],
    pub sig_pk: [u8; 32],
    pub cert: Cert,
}

/// The ephemeral plane: genesis ceremony + op log + fold cache.
/// Signing keys are re-derived from held seeds at each seal site (the
/// seeds are the ephemeral secret; the dalek type is never held).
pub(crate) struct EphemeralPlane {
    pub plane_id: [u8; 32],
    pub zone_id: [u8; 16],
    pub home_space: [u8; 16],
    #[allow(dead_code)]
    pub audit_space: [u8; 16],
    /// Held for post-genesis CONTROL ops (space creation, grants,
    /// curation ceremonies — later P1 slices); the genesis itself is
    /// sealed inside [`Self::bootstrap`].
    #[allow(dead_code)]
    root_seed: [u8; 32],
    #[allow(dead_code)]
    pub root_pk: [u8; 32],
    pub dev: WriterDevice,
    pub grant: Grant,
    /// name → signed op bytes; names are zero-padded append indexes,
    /// so `BTreeMap` order == append order.
    items: BTreeMap<String, Vec<u8>>,
    /// Fold cache over the CURRENT item set (recomputed on append —
    /// the fold is set-based and arrival-order-free by construction).
    verdicts: BTreeMap<String, Verdict>,
    state: State,
    /// The writer chain (single lineage, generation 1).
    writer_seq: u64,
    prev_writer_hash: [u8; 32],
    /// Control chain position (genesis holds seq 1).
    #[allow(dead_code)]
    ctrl_seq: u64,
    #[allow(dead_code)]
    ctrl_head: [u8; 32],
    /// HLC millisecond floor (strictly monotonic per plane).
    hlc_last_ms: u64,
}

impl EphemeralPlane {
    /// Mint the plane: the full `c.genesis` ceremony (§7.1 registry
    /// row; D-68/D-76 cross-field validity), then fold it and require
    /// admission by the stamped reader.
    pub(crate) fn bootstrap() -> Result<EphemeralPlane, MemoryError> {
        Ok(Self::bootstrap_with_custody()?.0)
    }

    /// [`Self::bootstrap`] plus the ceremony's custody-tier secrets —
    /// the create path of the P1.5 durable custody adapter
    /// (`memory::store`). Everything the ceremony mints randomly and
    /// then needs again at reopen rides [`PlaneCustody`]; the
    /// ephemeral path discards it.
    #[allow(dead_code)] // consumed by the store artifact + its battery until P1.8
    pub(crate) fn bootstrap_with_custody() -> Result<(EphemeralPlane, PlaneCustody), MemoryError> {
        let created_ms = now_ms();
        let root_seed = rand32();
        let (_, root_pk) = suite::ed25519::keypair(&root_seed);
        let recovery_seed = rand32();
        let (_, recovery_pk) = suite::ed25519::keypair(&recovery_seed);

        let descriptor = Genesis {
            root_sig_alg: Sigalg::Ed25519,
            root_sig_pk: root_pk.to_vec(),
            recovery_commitment: h_tag(Tag::Drill, &recovery_pk),
            provenance: Provenance::Trusted,
            created_ms,
        };
        let plane_id = h_tag(
            Tag::Genesis,
            &cbor::encode(&descriptor.to_value()).expect("descriptor encodes"),
        );

        // The daemon's writer device.
        let sig_seed = rand32();
        let (_, sig_pk) = suite::ed25519::keypair(&sig_seed);
        let kem_ikm = rand32();
        let (_, kem_pk) = hpke_wrap::derive_keypair(&kem_ikm);
        let device_id = rand_id();
        let lineage = rand_id();
        let cert = Cert {
            plane_id,
            device_id,
            sig_alg: Sigalg::Ed25519,
            sig_pk: sig_pk.to_vec(),
            kem_pk: kem_pk.to_vec(),
            class: Devclass::Daemon,
            evidence_hash: rand32(),
            evidence_media_type: None,
            issued_admin_epoch: 1,
            expiry_deadline_ms: None,
            revocation_id: rand_id(),
            renews: None,
        };
        let dev = WriterDevice {
            device_id,
            lineage,
            sig_seed,
            sig_pk,
            cert: cert.clone(),
        };

        let zone_id = rand_id();
        let home_space = rand_id();
        let audit_space = rand_id();
        let kek_e1 = rand32();
        let custody = PlaneCustody {
            root_seed,
            recovery_seed,
            sig_seed,
            kem_ikm,
            kek_epoch1: kek_e1,
        };
        let (enc, ct) = keyschedule::wrap_kek(&kem_pk, &plane_id, &zone_id, 1, &kek_e1, &rand32())
            .ok_or_else(|| {
                MemoryError::InvalidArg("genesis kek wrap: derived recipient key rejected".into())
            })?;
        let wrap = Kekwrap {
            plane_id,
            zone_id,
            epoch: 1,
            recipient_device: device_id,
            recipient_kem_key: suite::key_id("hpke-p256-v1", &kem_pk),
            enc,
            ct,
        };

        let grant = Grant {
            plane_id,
            grant_id: rand_id(),
            subject_device: device_id,
            lineage: Some(lineage),
            tenants: vec![GrantTenant::Memory],
            zone: ZoneSel::Zone(zone_id),
            spaces: SpacesSel::Spaces(vec![home_space]),
            ops: daemon_writer_verbs(),
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
            grant_id: rand_id(),
            spaces: SpacesSel::Spaces(vec![audit_space]),
            ops: vec![Verb::AuditWrite],
            ..grant.clone()
        };

        let body = Cgenesis {
            descriptor,
            cert,
            lineage: Lineagedef {
                lineage,
                device_id,
                max_generations: 8,
            },
            zone_id,
            zone_wraps: vec![wrap],
            home_space: Spacedef {
                space_id: home_space,
                zone_id,
                name_hash: rand32(),
                space_class: Spaceclass::Personal,
                class_minimum: Class::Private,
                status_policy: Polref {
                    id: "workflow-v1".into(),
                    version: 1,
                    hash: scenario::workflow_v1().hash(),
                },
            },
            audit_space: Spacedef {
                space_id: audit_space,
                zone_id,
                name_hash: rand32(),
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

        let hlc_last_ms = created_ms + 1;
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
            owner_plane_core::shapes::identity::Authproof::Genesis { genesis: plane_id },
            rand_id(),
            1,
            None,
            Hlc {
                ms: hlc_last_ms,
                count: 0,
            },
            Cgenesis::OP_TYPE,
        );
        let (root_sk, _) = suite::ed25519::keypair(&root_seed);
        let genesis_op = seal_op(header, body.to_value(), &OpSigner::Ed25519(&root_sk));
        let ctrl_head = genesis_op.op_hash();

        let mut plane = EphemeralPlane {
            plane_id,
            zone_id,
            home_space,
            audit_space,
            root_seed,
            root_pk,
            dev,
            grant,
            items: BTreeMap::new(),
            verdicts: BTreeMap::new(),
            state: State::default(),
            writer_seq: 0,
            prev_writer_hash: [0; 32],
            ctrl_seq: 1,
            ctrl_head,
            hlc_last_ms,
        };
        plane.prev_writer_hash = gen_start(&plane.dev.lineage, 1);
        plane.append(genesis_op)?;
        Ok((plane, custody))
    }

    /// The held op set (name → signed-op bytes; genesis included) —
    /// what the durable adapter persists at create time.
    #[allow(dead_code)] // consumed by the store artifact + its battery until P1.8
    pub(crate) fn held_items(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.items
    }

    /// Rebuild a WRITABLE plane from recovered custody + the recovered
    /// op set (the P1.5 durable adapter's reopen path). Admission of
    /// the recovered set rides the STAMPED fold — any rejected
    /// recovered op surfaces its named outcome verbatim — and the
    /// writer chain resumes from the fold's own chain head, never
    /// from a second parse of the log.
    #[allow(dead_code)] // consumed by the store artifact + its battery until P1.8
    pub(crate) fn resume(
        r: &PlaneResume,
        items: BTreeMap<String, Vec<u8>>,
    ) -> Result<EphemeralPlane, MemoryError> {
        let (_, root_pk) = suite::ed25519::keypair(&r.custody.root_seed);
        let (_, sig_pk) = suite::ed25519::keypair(&r.custody.sig_seed);
        let (_, kem_pk) = hpke_wrap::derive_keypair(&r.custody.kem_ikm);
        let cert = Cert {
            plane_id: r.plane_id,
            device_id: r.device_id,
            sig_alg: Sigalg::Ed25519,
            sig_pk: sig_pk.to_vec(),
            kem_pk: kem_pk.to_vec(),
            class: Devclass::Daemon,
            evidence_hash: r.evidence_hash,
            evidence_media_type: None,
            issued_admin_epoch: 1,
            expiry_deadline_ms: None,
            revocation_id: r.revocation_id,
            renews: None,
        };
        let dev = WriterDevice {
            device_id: r.device_id,
            lineage: r.lineage,
            sig_seed: r.custody.sig_seed,
            sig_pk,
            cert,
        };
        let grant = Grant {
            plane_id: r.plane_id,
            grant_id: r.grant_id,
            subject_device: r.device_id,
            lineage: Some(r.lineage),
            tenants: vec![GrantTenant::Memory],
            zone: ZoneSel::Zone(r.zone_id),
            spaces: SpacesSel::Spaces(vec![r.home_space]),
            ops: daemon_writer_verbs(),
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

        if items.is_empty() {
            return Err(MemoryError::InvalidArg(
                "resume needs at least the genesis operation".into(),
            ));
        }
        let (verdicts, state) = fold_set(&items, &BTreeMap::new())
            .map_err(|Unimplemented(why)| MemoryError::Unimplemented(why))?;
        for verdict in verdicts.values() {
            if let Verdict::Rejected(outcome, disposition) = verdict {
                return Err(MemoryError::Rejected {
                    outcome,
                    disposition,
                });
            }
        }

        let (writer_seq, prev_writer_hash) =
            match state.tenant_chain_head(&r.zone_id, &r.lineage, 1) {
                Some((next_seq, head)) => (next_seq - 1, head),
                None => (0, gen_start(&r.lineage, 1)),
            };
        // HLC floor: the chain head op's own clock, so monotonicity
        // holds even if the wall clock stepped backward across the
        // restart (next_hlc maxes with now() on top).
        let mut hlc_last_ms = 0;
        if writer_seq > 0 {
            for raw in items.values() {
                if let Ok(op) = owner_plane_reducer::envelope::parse_op(raw) {
                    if op.op_hash() == prev_writer_hash {
                        hlc_last_ms = op.header.created_ms;
                        break;
                    }
                }
            }
        }

        Ok(EphemeralPlane {
            plane_id: r.plane_id,
            zone_id: r.zone_id,
            home_space: r.home_space,
            audit_space: r.audit_space,
            root_seed: r.custody.root_seed,
            root_pk,
            dev,
            grant,
            items,
            verdicts,
            state,
            writer_seq,
            prev_writer_hash,
            ctrl_seq: 1,
            ctrl_head: r.genesis_hash,
            hlc_last_ms,
        })
    }

    fn next_hlc(&mut self) -> Hlc {
        let ms = now_ms().max(self.hlc_last_ms + 1);
        self.hlc_last_ms = ms;
        Hlc { ms, count: 0 }
    }

    fn next_name(&self) -> String {
        format!("op-{:06}", self.items.len() + 1)
    }

    /// Append a signed op and re-derive the fold over the full set.
    /// Admitted/Pending ops stay in the log and hold their positions;
    /// a Rejected op is removed (a failed operation exerts no
    /// precedence, D-112) and its named outcome surfaces verbatim.
    fn append(&mut self, op: Signedop) -> Result<[u8; 32], MemoryError> {
        let name = self.next_name();
        let op_hash = op.op_hash();
        self.items.insert(name.clone(), op.encode());
        match fold_set(&self.items, &BTreeMap::new()) {
            Ok((verdicts, state)) => {
                let verdict = verdicts
                    .get(&name)
                    .copied()
                    .unwrap_or(Verdict::Rejected("op-unknown", "quarantine-reject"));
                match verdict {
                    Verdict::Admitted => {
                        self.verdicts = verdicts;
                        self.state = state;
                        Ok(op_hash)
                    }
                    Verdict::Pending(outcome, disposition) => {
                        self.verdicts = verdicts;
                        self.state = state;
                        Err(MemoryError::Pending {
                            outcome,
                            disposition,
                        })
                    }
                    Verdict::Rejected(outcome, disposition) => {
                        self.items.remove(&name);
                        // Re-derive without the rejected op so the
                        // cache never reflects a set we don't hold.
                        let (verdicts, state) = fold_set(&self.items, &BTreeMap::new())
                            .map_err(|Unimplemented(why)| MemoryError::Unimplemented(why))?;
                        self.verdicts = verdicts;
                        self.state = state;
                        Err(MemoryError::Rejected {
                            outcome,
                            disposition,
                        })
                    }
                }
            }
            Err(Unimplemented(why)) => {
                self.items.remove(&name);
                Err(MemoryError::Unimplemented(why))
            }
        }
    }

    /// Seal a Memory-tenant op on the writer chain and append it.
    /// On admission the chain advances; on rejection it does not (the
    /// failed op's coordinates stay reusable, D-112).
    pub(crate) fn tenant_op(
        &mut self,
        actor_kind: ActorKind,
        actor_id: Option<String>,
        op_type: &str,
        body: cbor::Value,
    ) -> Result<[u8; 32], MemoryError> {
        let seq = self.writer_seq + 1;
        let hlc = self.next_hlc();
        let header = Header {
            tenant: Tenant::Memory,
            plane_id: self.plane_id,
            zone_id: self.zone_id,
            space_id: self.home_space,
            authored_kek_epoch: 1,
            capability_epoch: 1,
            signer_alg: Sigalg::Ed25519,
            signer_key_id: suite::key_id("ed25519", &self.dev.sig_pk),
            writer: Writer {
                lineage: self.dev.lineage,
                gen: 1,
            },
            actor: Actor {
                kind: actor_kind,
                id: actor_id.unwrap_or_else(|| hex(&self.dev.device_id)),
                attested_by: None,
            },
            authorization_proof: owner_plane_core::shapes::identity::Authproof::Dev {
                cert: h_cert(&self.dev.cert),
                cap: h_grant(&self.grant),
            },
            request_id: rand_id(),
            writer_sequence: seq,
            previous_writer_hash: self.prev_writer_hash,
            causal_references: vec![],
            created_hlc: hlc,
            operation_type: op_type.into(),
            operation_version: 1,
            body_hash: [0; 32], // set by seal_op
        };
        let (dev_sk, _) = suite::ed25519::keypair(&self.dev.sig_seed);
        let op = seal_op(header, body, &OpSigner::Ed25519(&dev_sk));
        let op_hash = self.append(op)?;
        self.writer_seq = seq;
        self.prev_writer_hash = op_hash;
        Ok(op_hash)
    }

    /// Derived claim status via the reducer's §11.2 fold — never a
    /// service-side reimplementation.
    pub(crate) fn claim_status(&self, op_hash: &[u8; 32]) -> Option<&'static str> {
        self.state.claim_status(op_hash, now_ms())
    }

    /// Number of held (admitted or pending) operations.
    #[cfg(test)]
    pub(crate) fn held_ops(&self) -> usize {
        self.items.len()
    }
}
