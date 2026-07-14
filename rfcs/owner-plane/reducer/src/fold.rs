//! The fold engine — §10.2 admission over delivered operations.
//!
//! A work-queue fold: items arrive in delivery order; each is
//! classified (admitted / pending / rejected); every acceptance
//! re-evaluates the pending set to fixpoint (the reservation
//! pattern — control order survives pendency). The engine implements
//! EXACTLY what it knows: an operation type outside its registry
//! coverage aborts the vector as `Unimplemented` rather than guessing
//! — the tranche burns down fixture by fixture as coverage grows.
//!
//! Scope so far: `c.genesis` (the §7.1 row's D-68 cross-field rules),
//! `c.enroll` (new-device shape: chain, one-live-lineage,
//! import-grant uniqueness, the exact-SEC1 freshness domain — D-190's
//! acceptance side), `m.claim` under the dev arm (D-199: unresolved
//! certificate/grant citations pend `ref-unresolved` and admit on
//! arrival), and the revocation compound: `c.grant` (D-92/D-139
//! issuance gates), `c.revoke_grant` (D-93 cutoff equality),
//! `c.revoke_device` in exclude mode (the D-180/D-186 one completion
//! law over the D-173 decryptable-wrap domain, with the D-195
//! reservation — a pending compound HOLDS its chain position, unlike
//! a failed op which exerts no precedence, D-112), `c.kek_rotate`
//! (dense epochs, wrap-set validation, the D-81 last-holder floor),
//! and the staging machine: `c.cutoff`'s requesterless `closes` lane
//! (D-136) plus `c.cap_epoch_bump` under the union-coverage rule —
//! stages consume one-shot at the advance (D-153) and die vacuously
//! at an authority-ending frontier (D-196).

use std::collections::BTreeMap;

use crate::cbor::Node;
use crate::domains;
use crate::envelope::{parse_op, Proof, SignedOp};

pub const CTRL_ZONE: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
pub const CTRL_SPACE: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
pub const CTRL_LINEAGE: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3];

/// A classification the fold can hold for an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Admitted,
    Pending(&'static str, &'static str),
    Rejected(&'static str, &'static str),
}

impl Verdict {
    pub fn pair(&self) -> Option<(&'static str, &'static str)> {
        match self {
            Verdict::Admitted => None,
            Verdict::Pending(o, d) | Verdict::Rejected(o, d) => Some((o, d)),
        }
    }
}

/// The engine met something outside its implemented registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unimplemented(pub String);

#[derive(Debug, Clone)]
struct HeldCert {
    h_cert: [u8; 32],
    device_id: [u8; 16],
    sig_pk: [u8; 32],
    /// `H_key({kem_alg, kem_pk})` — what a wrap's `recipient_kem_key`
    /// must equal.
    kem_key_id: [u8; 32],
    revocation_id: [u8; 16],
    /// Set at a `c.revoke_device` compound's COMPLETING acceptance
    /// (D-195 — a pending compound ends nothing yet).
    revoked: bool,
}

#[derive(Debug, Clone)]
struct HeldGrant {
    h_grant: [u8; 32],
    grant_id: [u8; 16],
    subject_device: [u8; 16],
    lineage: Option<[u8; 16]>,
    zone: Option<[u8; 16]>,
    spaces: Option<Vec<[u8; 16]>>,
    verbs: Vec<String>,
    tenants: Vec<String>,
    kinds: Option<Vec<String>>,
    capability_epoch: u64,
    imports: bool,
    /// `c.revoke_grant`, or derived revocation at a device compound's
    /// completion (D-85).
    revoked: bool,
}

/// §11.1 (D-60): the verbs whose operations append tenant chain
/// state. A grant carrying any of them requires `lineage` and exactly
/// one finite zone (D-32).
const OP_AUTHORING: &[&str] = &[
    "propose",
    "assert",
    "judge.safe",
    "judge.full",
    "pin.safe",
    "pin.full",
    "erase.request",
    "raise",
    "declassify",
    "export",
    "import",
    "audit.write",
];

/// §11.1's closed grant-verb vocabulary.
const VERBS: &[&str] = &[
    "search",
    "read",
    "evidence.read",
    "propose",
    "assert",
    "judge.safe",
    "judge.full",
    "pin.safe",
    "pin.full",
    "erase.request",
    "raise",
    "declassify",
    "export",
    "import",
    "curate.instruction",
    "audit.write",
    "admin",
];

/// (zone, lineage, gen) — one tenant chain's coordinates.
type ChainKey = ([u8; 16], [u8; 16], u64);
/// (next expected seq, current head op hash).
type ChainHead = (u64, [u8; 32]);
/// A frontierclose's (zone, lineage) coordinates.
type ZoneLineage = ([u8; 16], [u8; 16]);

/// Derived plane state — grown only by ACCEPTED operations.
#[derive(Debug, Clone, Default)]
pub struct State {
    plane_id: Option<[u8; 32]>,
    root_pk: Option<[u8; 32]>,
    ctrl_next_seq: u64,
    ctrl_head: [u8; 32],
    zones: Vec<[u8; 16]>,
    spaces: Vec<([u8; 16], [u8; 16])>, // (space_id, zone_id)
    certs: Vec<HeldCert>,
    grants: Vec<HeldGrant>,
    lineages: Vec<([u8; 16], [u8; 16])>, // (lineage, device_id)
    /// Exact-SEC1 freshness domain: key_ids and mat_ids of every
    /// enrolled certificate's keys.
    freshness: Vec<[u8; 32]>,
    /// Tenant chain heads: (zone, lineage, gen) → (next_seq, head op).
    tenant_chains: BTreeMap<ChainKey, ChainHead>,
    /// zone → latest accepted KEK epoch (dense from 1, §5.5).
    kek_epochs: BTreeMap<[u8; 16], u64>,
    /// (zone, epoch) → recipient devices holding an effective wrap
    /// there (re-wraps supersede by `(zone, epoch, device)`, so
    /// membership is a set of devices).
    wrap_sets: BTreeMap<([u8; 16], u64), Vec<[u8; 16]>>,
    /// Pending `c.revoke_device` compounds that already HOLD their
    /// control position (the reservation — the chain continues past a
    /// pending compound; only the compound's own effects wait):
    /// op_hash → target revocation_id.
    pending_compounds: BTreeMap<[u8; 32], [u8; 16]>,
    /// Completed (effect-applied) revocation_ids.
    revoked_ids: Vec<[u8; 16]>,
    /// zone → current capability epoch (opens at 1, §9.4).
    cap_epochs: BTreeMap<[u8; 16], u64>,
    /// zone → `zone_policy.strictness == "strict"` (the union-coverage
    /// rule binds under strict).
    zone_strict: BTreeMap<[u8; 16], bool>,
    /// UNCONSUMED staged frontier closures (`ccutoff.closes`, D-136)
    /// — inert until a consuming advance materializes them; one-shot
    /// (D-153), vacuously consumed by an authority-ending frontier
    /// (D-196).
    staged_closes: Vec<ZoneLineage>,
}

fn ok<T>(v: T) -> Result<T, Unimplemented> {
    Ok(v)
}

fn b16_field(n: &Node, key: &str) -> Option<[u8; 16]> {
    n.get(key)?.bytes_n::<16>()
}

impl State {
    /// O7 pins common to every control operation.
    fn ctrl_header_pins(op: &SignedOp) -> Result<(), Verdict> {
        let h = &op.header;
        if h.tenant != "ctrl"
            || h.zone_id != CTRL_ZONE
            || h.space_id != CTRL_SPACE
            || h.writer_lineage != CTRL_LINEAGE
            || h.writer_gen != 1
            || h.authored_kek_epoch != 0
            || h.capability_epoch != 0
            || h.actor_kind != "human"
            || h.actor_id != "owner"
            || h.attested_by.is_some()
        {
            return Err(Verdict::Rejected("body-invariant", "reject-permanent"));
        }
        Ok(())
    }

    /// §9.3 chain arithmetic on the control chain. `Pending` = the
    /// gap-successor case (causal-missing).
    fn ctrl_chain(&self, op: &SignedOp) -> Result<(), Verdict> {
        let h = &op.header;
        let expect_seq = self.ctrl_next_seq.max(1);
        match h.writer_sequence.cmp(&expect_seq) {
            std::cmp::Ordering::Less => {
                // A duplicate position: byte-identical replay would be
                // `duplicate`; a different op at a held position is a
                // C2 question. Not exercised by the tranche's accepted
                // paths — the D-112 rejected-candidate case never
                // holds the position, so a SECOND op at the same seq
                // arrives with expect_seq still there.
                Err(Verdict::Rejected("ctrl-fork", "freeze-control"))
            }
            std::cmp::Ordering::Greater => {
                Err(Verdict::Pending("causal-missing", "pending-dependency"))
            }
            std::cmp::Ordering::Equal => {
                let want_prev = if expect_seq == 1 {
                    domains::gen_start(&CTRL_LINEAGE, 1)
                } else {
                    self.ctrl_head
                };
                if h.previous_writer_hash != want_prev {
                    return Err(Verdict::Rejected("fork", "freeze-writer"));
                }
                Ok(())
            }
        }
    }

    /// Admin-arm resolution: the root key IS the epoch-1 admin key
    /// (no succession in tranche state yet).
    fn admin_key(&self, epoch: u64) -> Result<[u8; 32], Verdict> {
        if epoch != 1 {
            return Err(Verdict::Rejected("proof-arm", "reject-permanent"));
        }
        self.root_pk
            .ok_or(Verdict::Pending("ref-unresolved", "pending-dependency"))
    }

    fn record_cert(&mut self, cert_node: &Node) -> Result<(), Unimplemented> {
        let h_cert = domains::h("cert", cert_node.raw);
        let sig_pk_raw = cert_node
            .get("sig_pk")
            .and_then(|n| n.as_bytes())
            .unwrap_or_default();
        let kem_pk = cert_node
            .get("kem_pk")
            .and_then(|n| n.as_bytes())
            .unwrap_or_default();
        let sig_alg = cert_node
            .get("sig_alg")
            .and_then(|n| n.as_text())
            .unwrap_or_default();
        self.freshness.push(domains::key_id(sig_alg, sig_pk_raw));
        self.freshness.push(domains::key_id("hpke-p256-v1", kem_pk));
        self.freshness.push(domains::h("mat", kem_pk));
        if sig_alg == "p256" {
            self.freshness.push(domains::h("mat", sig_pk_raw));
        }
        self.certs.push(HeldCert {
            h_cert,
            device_id: b16_field(cert_node, "device_id").unwrap_or_default(),
            sig_pk: cert_node
                .get("sig_pk")
                .and_then(|n| n.bytes_n::<32>())
                .unwrap_or_default(),
            kem_key_id: domains::key_id("hpke-p256-v1", kem_pk),
            revocation_id: b16_field(cert_node, "revocation_id").unwrap_or_default(),
            revoked: false,
        });
        ok(())
    }

    fn record_grant(&mut self, grant_node: &Node) -> Result<(), Unimplemented> {
        let verbs = grant_node
            .get("ops")
            .and_then(|n| n.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_text().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let tenants = grant_node
            .get("tenants")
            .and_then(|n| n.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_text().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let zone = grant_node.get("zone").and_then(|z| {
            if z.as_text() == Some("*") {
                None
            } else {
                z.bytes_n::<16>()
            }
        });
        let spaces = grant_node.get("spaces").and_then(|s| {
            if s.as_text() == Some("*") {
                None
            } else {
                s.as_array().map(|a| {
                    a.iter()
                        .filter_map(|v| v.bytes_n::<16>())
                        .collect::<Vec<_>>()
                })
            }
        });
        let kinds = grant_node.get("kinds").and_then(|k| {
            k.as_array().map(|a| {
                a.iter()
                    .filter_map(|v| v.as_text().map(str::to_string))
                    .collect::<Vec<_>>()
            })
        });
        self.grants.push(HeldGrant {
            h_grant: domains::h("grant", grant_node.raw),
            grant_id: b16_field(grant_node, "grant_id").unwrap_or_default(),
            subject_device: b16_field(grant_node, "subject_device").unwrap_or_default(),
            lineage: b16_field(grant_node, "lineage"),
            zone,
            spaces,
            imports: verbs.iter().any(|v| v == "import"),
            verbs,
            tenants,
            kinds,
            capability_epoch: grant_node
                .get("capability_epoch")
                .and_then(|n| n.as_uint())
                .unwrap_or(0),
            revoked: false,
        });
        ok(())
    }

    /// Universal grant-object gates shared by every grant-bearing
    /// operation (`c.grant` AND `c.enroll.grants[]`): the closed §11.1
    /// verb vocabulary, the reserved `admin` verb (D-61: rejects at
    /// issuance), and D-60/D-32 — an op-authoring grant carries a
    /// `lineage` and exactly ONE finite zone (`"*"` is read-only),
    /// and the subject device owns the named lineage. `enrolling` is
    /// the `(lineage, device)` the CURRENT operation creates (genesis
    /// and enroll grants ride the op that mints their lineage).
    fn grant_static_checks(
        &self,
        gn: &Node,
        plane: [u8; 32],
        enrolling: Option<([u8; 16], [u8; 16])>,
    ) -> Option<Verdict> {
        let bad = Some(Verdict::Rejected("body-invariant", "reject-permanent"));
        if gn.get("plane_id").and_then(|n| n.bytes_n::<32>()) != Some(plane) {
            return bad;
        }
        let verbs: Vec<&str> = gn
            .get("ops")
            .and_then(|n| n.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_text()).collect())
            .unwrap_or_default();
        if verbs.is_empty() || verbs.iter().any(|v| !VERBS.contains(v)) || verbs.contains(&"admin")
        {
            return bad;
        }
        if verbs.iter().any(|v| OP_AUTHORING.contains(v)) {
            let zone_finite = gn.get("zone").is_some_and(|z| z.bytes_n::<16>().is_some());
            let owned = match (b16_field(gn, "lineage"), b16_field(gn, "subject_device")) {
                (Some(l), Some(s)) => {
                    enrolling == Some((l, s))
                        || self.lineages.iter().any(|(li, d)| *li == l && *d == s)
                }
                _ => false,
            };
            if !zone_finite || !owned {
                return bad;
            }
        }
        None
    }

    /// Validate one `kekwrap` node against its context and return the
    /// recipient device. `expect_recipient` pins the recipient (the
    /// genesis/enroll shapes, D-76); `None` (rotations) requires a
    /// held certificate and checks the KEM key against it.
    fn check_wrap(
        &self,
        wn: &Node,
        plane: [u8; 32],
        zone: [u8; 16],
        epoch: u64,
        expect_recipient: Option<([u8; 16], [u8; 32])>,
    ) -> Result<Result<[u8; 16], Verdict>, Unimplemented> {
        let bad = || Verdict::Rejected("body-invariant", "reject-permanent");
        if wn.get("v").and_then(|n| n.as_uint()) != Some(1)
            || wn.get("kem").and_then(|n| n.as_text()) != Some("hpke-p256-v1")
            || wn.get("plane_id").and_then(|n| n.bytes_n::<32>()) != Some(plane)
            || b16_field(wn, "zone_id") != Some(zone)
            || wn.get("epoch").and_then(|n| n.as_uint()) != Some(epoch)
        {
            return ok(Err(bad()));
        }
        let Some(recipient) = b16_field(wn, "recipient_device") else {
            return ok(Err(bad()));
        };
        let kem_key = wn.get("recipient_kem_key").and_then(|n| n.bytes_n::<32>());
        match expect_recipient {
            Some((device, key_id)) => {
                if recipient != device || kem_key != Some(key_id) {
                    return ok(Err(bad()));
                }
            }
            None => {
                let Some(cert) = self.certs.iter().find(|c| c.device_id == recipient) else {
                    // The recipient's enrollment may still arrive
                    // (interpretation: unheld recipient pends —
                    // register #24; no vector pins it yet).
                    return ok(Err(Verdict::Pending(
                        "ref-unresolved",
                        "pending-dependency",
                    )));
                };
                if kem_key != Some(cert.kem_key_id) {
                    return ok(Err(bad()));
                }
            }
        }
        ok(Ok(recipient))
    }

    /// Add `device` to the `(zone, epoch)` recipient set (idempotent —
    /// a re-wrap supersedes by `(zone, epoch, device)`).
    fn record_wrap(&mut self, zone: [u8; 16], epoch: u64, device: [u8; 16]) {
        let set = self.wrap_sets.entry((zone, epoch)).or_default();
        if !set.contains(&device) {
            set.push(device);
        }
    }

    /// D-151: the zone's LIVE lineages — those with an active
    /// op-authoring grant naming the zone.
    fn live_lineages(&self, zone: [u8; 16]) -> Vec<[u8; 16]> {
        let mut out = Vec::new();
        for g in self
            .grants
            .iter()
            .filter(|g| !g.revoked && g.zone == Some(zone))
        {
            if !g.verbs.iter().any(|v| OP_AUTHORING.contains(&v.as_str())) {
                continue;
            }
            if let Some(l) = g.lineage {
                if !out.contains(&l) {
                    out.push(l);
                }
            }
        }
        out
    }

    /// D-196: an authority-ending frontier VACUOUSLY CONSUMES the
    /// unconsumed stages of the lineages it removed from the coverage
    /// domain — one-shot-spent at the ending acceptance, so a later
    /// regrant cannot resurrect them.
    fn consume_dead_stages(&mut self, ended: &[ZoneLineage]) {
        for &(z, l) in ended {
            if !self.live_lineages(z).contains(&l) {
                self.staged_closes.retain(|&(sz, sl)| !(sz == z && sl == l));
            }
        }
    }

    /// `c.genesis` — control seq 1 only, genesis arm, D-68
    /// cross-field validity over the carried objects.
    fn admit_genesis(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        if let Err(v) = Self::ctrl_header_pins(op) {
            return ok(Err(v));
        }
        if self.plane_id.is_some() || op.header.writer_sequence != 1 {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }
        if op.header.previous_writer_hash != domains::gen_start(&CTRL_LINEAGE, 1) {
            return ok(Err(Verdict::Rejected("fork", "freeze-writer")));
        }
        // The self-contained genesis composition (root key from the
        // descriptor, N4 plane identity, arm citation, signature).
        if op.verify_genesis().is_err() {
            return ok(Err(Verdict::Rejected("sig-invalid", "reject-permanent")));
        }
        let body = &op.body;
        let (Some(descriptor), Some(cert), Some(lineage), Some(zone)) = (
            body.get("descriptor"),
            body.get("cert"),
            body.get("lineage"),
            body.get("zone"),
        ) else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        // D-68 cross-field spine (the tranche's geneses are valid;
        // negatives arrive with the corpus).
        let device_id = b16_field(cert, "device_id");
        let lineage_dev = b16_field(lineage, "device_id");
        let lineage_id = b16_field(lineage, "lineage");
        let zone_id = b16_field(zone, "zone_id");
        if device_id.is_none() || device_id != lineage_dev || lineage_id.is_none() {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }
        let (Some(home), Some(audit)) = (body.get("home_space"), body.get("audit_space")) else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        let home_id = b16_field(home, "space_id");
        let audit_id = b16_field(audit, "space_id");
        if home_id.is_none() || home_id == audit_id {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }
        let root_pk: [u8; 32] = descriptor
            .get("root_sig_pk")
            .and_then(|n| n.bytes_n::<32>())
            .expect("verify_genesis proved shape");

        // The zone opens at KEK epoch 1 with the wrap to the first
        // device (row pins: zone_id/epoch/recipient/recipient_kem_key;
        // verify_genesis proved header.plane_id = H_genesis(descriptor)).
        let plane = op.header.plane_id;
        if zone.get("initial_epoch").and_then(|n| n.as_uint()) != Some(1) {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }
        let kem_key_id = domains::key_id(
            "hpke-p256-v1",
            cert.get("kem_pk").and_then(|n| n.as_bytes()).unwrap_or(&[]),
        );
        let mut recipients = Vec::new();
        for wn in zone.get("wraps").and_then(|n| n.as_array()).unwrap_or(&[]) {
            match self.check_wrap(
                wn,
                plane,
                zone_id.unwrap_or_default(),
                1,
                Some((device_id.unwrap(), kem_key_id)),
            )? {
                Ok(r) => recipients.push(r),
                Err(v) => return ok(Err(v)),
            }
        }
        for g in ["grant", "audit_grant"] {
            let enrolling = Some((lineage_id.unwrap(), device_id.unwrap()));
            if let Some(v) = body
                .get(g)
                .and_then(|gn| self.grant_static_checks(gn, plane, enrolling))
            {
                return ok(Err(v));
            }
        }

        // Accept: install the plane. KEK epoch 1 AND capability
        // epoch 1 open here (§7.1 row); the B.1 policy's strictness
        // scopes the union-coverage rule.
        self.plane_id = Some(plane);
        self.root_pk = Some(root_pk);
        self.zones.push(zone_id.unwrap_or_default());
        self.kek_epochs.insert(zone_id.unwrap_or_default(), 1);
        self.cap_epochs.insert(zone_id.unwrap_or_default(), 1);
        let strict = body
            .get("zone_policy")
            .and_then(|p| p.get("strictness"))
            .and_then(|s| s.as_text())
            == Some("strict");
        self.zone_strict.insert(zone_id.unwrap_or_default(), strict);
        for r in recipients {
            self.record_wrap(zone_id.unwrap_or_default(), 1, r);
        }
        self.spaces
            .push((home_id.unwrap(), zone_id.unwrap_or_default()));
        self.spaces
            .push((audit_id.unwrap(), zone_id.unwrap_or_default()));
        self.lineages
            .push((lineage_id.unwrap(), device_id.unwrap()));
        self.record_cert(cert)?;
        for g in ["grant", "audit_grant"] {
            if let Some(gn) = body.get(g) {
                self.record_grant(gn)?;
            }
        }
        self.ctrl_next_seq = 2;
        self.ctrl_head = op.op_hash();
        ok(Ok(()))
    }

    /// The shared post-genesis admin-arm preamble: O7 pins, §9.3
    /// chain arithmetic, admin-key resolution, signature, body hash.
    fn ctrl_admin_preamble(&self, op: &SignedOp) -> Result<(), Verdict> {
        Self::ctrl_header_pins(op)?;
        self.ctrl_chain(op)?;
        let Proof::Admin { epoch, .. } = op.header.proof else {
            return Err(Verdict::Rejected("proof-arm", "reject-permanent"));
        };
        let admin_pk = self.admin_key(epoch)?;
        if !op.verify_ed25519(&admin_pk)
            || op.header.signer_key_id != domains::key_id("ed25519", &admin_pk)
        {
            return Err(Verdict::Rejected("sig-invalid", "reject-permanent"));
        }
        if !op.body_hash_ok() {
            return Err(Verdict::Rejected("body-hash", "reject-permanent"));
        }
        Ok(())
    }

    /// `c.enroll`, new-device shape (`cert.renews` absent).
    fn admit_enroll(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        if let Err(v) = self.ctrl_admin_preamble(op) {
            return ok(Err(v));
        }
        let body = &op.body;
        let Some(cert) = body.get("cert") else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        if cert.get("renews").is_some() {
            return Err(Unimplemented("cenrollrenew".into()));
        }
        let Some(device_id) = b16_field(cert, "device_id") else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };

        // Freshness: exact-SEC1 typed domain (D-190's boundary — the
        // negation of an enrolled point is OUTSIDE it and admits).
        let sig_alg = cert.get("sig_alg").and_then(|n| n.as_text()).unwrap_or("");
        let sig_pk = cert.get("sig_pk").and_then(|n| n.as_bytes()).unwrap_or(&[]);
        let kem_pk = cert.get("kem_pk").and_then(|n| n.as_bytes()).unwrap_or(&[]);
        let mut candidate_ids = vec![
            domains::key_id(sig_alg, sig_pk),
            domains::key_id("hpke-p256-v1", kem_pk),
            domains::h("mat", kem_pk),
        ];
        if sig_alg == "p256" {
            candidate_ids.push(domains::h("mat", sig_pk));
            if sig_pk == kem_pk {
                // Intra-certificate role reuse (D-175).
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            }
        }
        if candidate_ids.iter().any(|id| self.freshness.contains(id)) {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }

        // One live lineage per device.
        let Some(lineage) = body.get("lineage") else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        let Some(lineage_id) = b16_field(lineage, "lineage") else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        if b16_field(lineage, "device_id") != Some(device_id)
            || self.lineages.iter().any(|(_, d)| *d == device_id)
        {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }

        // Grants: every entry targets the enrolled device; the
        // universal grant gates apply (the invariant binds EVERY
        // grant-bearing operation); a second active import-verb grant
        // for a destination zone rejects (D-139/D-146).
        let plane = self
            .plane_id
            .expect("admin key resolved ⇒ genesis installed");
        let mut new_grants = Vec::new();
        if let Some(grants) = body.get("grants").and_then(|g| g.as_array()) {
            for gn in grants {
                if b16_field(gn, "subject_device") != Some(device_id) {
                    return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
                }
                if let Some(v) = self.grant_static_checks(gn, plane, Some((lineage_id, device_id)))
                {
                    return ok(Err(v));
                }
                let has_import = gn
                    .get("ops")
                    .and_then(|o| o.as_array())
                    .is_some_and(|a| a.iter().any(|v| v.as_text() == Some("import")));
                if has_import {
                    let gzone = gn.get("zone").and_then(|z| z.bytes_n::<16>());
                    if self
                        .grants
                        .iter()
                        .any(|g| g.imports && !g.revoked && g.zone == gzone)
                    {
                        return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
                    }
                }
                new_grants.push(gn.clone());
            }
        }

        // Wraps: each targets the enrolled device (D-76) at a known
        // zone's CURRENT accepted epoch (the only shape the tranche
        // mints — other epochs are unpinned, honest abort).
        let kem_key_id = domains::key_id("hpke-p256-v1", kem_pk);
        let mut new_wraps = Vec::new();
        if let Some(wraps) = body.get("wraps").and_then(|w| w.as_array()) {
            for wn in wraps {
                let Some(wz) = b16_field(wn, "zone_id") else {
                    return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
                };
                let Some(&cur) = self.kek_epochs.get(&wz) else {
                    return Err(Unimplemented("enroll wrap for unknown zone".into()));
                };
                if wn.get("epoch").and_then(|n| n.as_uint()) != Some(cur) {
                    return Err(Unimplemented("enroll wrap at non-current epoch".into()));
                }
                match self.check_wrap(wn, plane, wz, cur, Some((device_id, kem_key_id)))? {
                    Ok(r) => new_wraps.push((wz, cur, r)),
                    Err(v) => return ok(Err(v)),
                }
            }
        }

        // Accept.
        self.lineages.push((lineage_id, device_id));
        self.record_cert(cert)?;
        for gn in &new_grants {
            self.record_grant(gn)?;
        }
        for (z, e, d) in new_wraps {
            self.record_wrap(z, e, d);
        }
        self.ctrl_next_seq += 1;
        self.ctrl_head = op.op_hash();
        ok(Ok(()))
    }

    /// `c.grant` — issue one capability. Row gates implemented:
    /// D-92 (issuance to a revoked device rejects), D-139 (one active
    /// import-verb grant per destination zone), the universal grant
    /// object gates. Deliberately deferred to later slices (their
    /// state does not exist yet; corpus vectors pin them): the D-109
    /// 129-held-zone cap, capability-epoch currency, and the
    /// budget-required-under-`budgets`-policy rule.
    fn admit_grant(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        if let Err(v) = self.ctrl_admin_preamble(op) {
            return ok(Err(v));
        }
        let Some(gn) = op.body.get("grant") else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        let plane = self
            .plane_id
            .expect("admin key resolved ⇒ genesis installed");
        if let Some(v) = self.grant_static_checks(gn, plane, None) {
            return ok(Err(v));
        }
        let Some(subject) = b16_field(gn, "subject_device") else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        let Some(cert) = self.certs.iter().find(|c| c.device_id == subject) else {
            // The subject's enrollment may arrive later (D-199
            // spirit; interpretation register #25 — unpinned).
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        // D-92: issuance to a device whose revocation_id is REVOKED
        // rejects. A pending compound deactivates nothing (D-195 —
        // the window; this tranche's window grant admits).
        if self.revoked_ids.contains(&cert.revocation_id) {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }
        let has_import = gn
            .get("ops")
            .and_then(|o| o.as_array())
            .is_some_and(|a| a.iter().any(|v| v.as_text() == Some("import")));
        if has_import {
            let gzone = gn.get("zone").and_then(|z| z.bytes_n::<16>());
            if self
                .grants
                .iter()
                .any(|g| g.imports && !g.revoked && g.zone == gzone)
            {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            }
        }

        // Accept.
        self.record_grant(gn)?;
        self.ctrl_next_seq += 1;
        self.ctrl_head = op.op_hash();
        ok(Ok(()))
    }

    /// `c.revoke_grant` — an op-authoring grant's revocation carries
    /// a REQUIRED `frontierclose` naming that grant's zone and
    /// lineage exactly (D-78/D-143, equality D-93).
    fn admit_revoke_grant(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        if let Err(v) = self.ctrl_admin_preamble(op) {
            return ok(Err(v));
        }
        let body = &op.body;
        let Some(gid) = b16_field(body, "grant_id") else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        let Some(idx) = self.grants.iter().position(|g| g.grant_id == gid) else {
            // Unheld grant citation — the issuance may arrive later
            // (interpretation register #25 — unpinned).
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        if self.grants[idx].revoked {
            return Err(Unimplemented("re-revocation of a revoked grant".into()));
        }
        let op_authoring = self.grants[idx]
            .verbs
            .iter()
            .any(|v| OP_AUTHORING.contains(&v.as_str()));
        let cutoff = body.get("cutoff");
        if op_authoring {
            let Some(cn) = cutoff else {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            };
            if b16_field(cn, "zone_id") != self.grants[idx].zone
                || b16_field(cn, "lineage") != self.grants[idx].lineage
            {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            }
            match cn.get("heads").and_then(|h| h.as_array()) {
                None => return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent"))),
                Some(a) if !a.is_empty() => {
                    return Err(Unimplemented("frontierclose heads".into()))
                }
                Some(_) => {}
            }
        } else if cutoff.is_some() {
            return Err(Unimplemented(
                "cutoff on a read-only grant revocation".into(),
            ));
        }

        // Accept. (The boundary's quarantine consumers arrive with
        // later slices; state records the deactivation.)
        let ended = match (self.grants[idx].zone, self.grants[idx].lineage) {
            (Some(z), Some(l)) if op_authoring => vec![(z, l)],
            _ => vec![],
        };
        self.grants[idx].revoked = true;
        self.consume_dead_stages(&ended);
        self.ctrl_next_seq += 1;
        self.ctrl_head = op.op_hash();
        ok(Ok(()))
    }

    /// Parse a compound's `cutoffs` into `(zone, lineage)` pairs.
    /// Only the empty-heads shape is implemented (D-143 — the shape
    /// the tranche mints; carried heads await their consumer slice).
    fn compound_cutoffs(body: &Node) -> Result<Result<Vec<ZoneLineage>, Verdict>, Unimplemented> {
        let mut out = Vec::new();
        let Some(cs) = body.get("cutoffs").and_then(|c| c.as_array()) else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        for cn in cs {
            let (Some(z), Some(l)) = (b16_field(cn, "zone_id"), b16_field(cn, "lineage")) else {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            };
            match cn.get("heads").and_then(|h| h.as_array()) {
                None => return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent"))),
                Some(a) if !a.is_empty() => {
                    return Err(Unimplemented("frontierclose heads".into()))
                }
                Some(_) => out.push((z, l)),
            }
        }
        ok(Ok(out))
    }

    /// Evaluate the one completion law (D-180/D-186) at the current
    /// position and, when it holds, apply the compound's effects: the
    /// certificates cease HERE (D-195), grant revocation is derived
    /// (D-85). Incomplete → `ref-unresolved` (awaiting completing
    /// exclusions/cutoffs).
    fn try_complete_compound(
        &mut self,
        oh: [u8; 32],
        rid: [u8; 16],
        cutoffs: &[ZoneLineage],
    ) -> Result<(), Verdict> {
        let pend = Verdict::Pending("ref-unresolved", "pending-dependency");
        let targets: Vec<[u8; 16]> = self
            .certs
            .iter()
            .filter(|c| c.revocation_id == rid)
            .map(|c| c.device_id)
            .collect();
        // (2) Authorship-domain totality (D-159/D-141): every zone
        // named by the targets' active op-authoring grants has a
        // cutoff naming it and the target lineage.
        for g in self
            .grants
            .iter()
            .filter(|g| !g.revoked && targets.contains(&g.subject_device))
        {
            if !g.verbs.iter().any(|v| OP_AUTHORING.contains(&v.as_str())) {
                continue;
            }
            let (Some(zone), Some(lineage)) = (g.zone, g.lineage) else {
                // Op-authoring grants carry a finite zone + lineage
                // (issuance-gated) — unreachable for held state, but
                // pend rather than assert.
                return Err(pend);
            };
            if !cutoffs.contains(&(zone, lineage)) {
                return Err(pend);
            }
        }
        // (3) The decryptable-wrap domain (D-173) is EMPTY: no zone
        // has an accepted epoch at which a target holds an effective
        // wrap not already followed by an accepted rotation excluding
        // it (the row's literal predicate — the current-membership
        // shortcut reading was voided by D-173).
        for d in &targets {
            for (&zone, &cur) in &self.kek_epochs {
                let in_domain = (1..=cur).any(|e| {
                    let holds = self
                        .wrap_sets
                        .get(&(zone, e))
                        .is_some_and(|r| r.contains(d));
                    holds
                        && ((e + 1)..=cur).all(|e2| {
                            self.wrap_sets
                                .get(&(zone, e2))
                                .is_some_and(|r| r.contains(d))
                        })
                });
                if in_domain {
                    return Err(pend);
                }
            }
        }
        // Complete.
        for c in self.certs.iter_mut().filter(|c| c.revocation_id == rid) {
            c.revoked = true;
        }
        let mut ended: Vec<ZoneLineage> = Vec::new();
        for g in self
            .grants
            .iter_mut()
            .filter(|g| !g.revoked && targets.contains(&g.subject_device))
        {
            g.revoked = true;
            if let (Some(z), Some(l)) = (g.zone, g.lineage) {
                if g.verbs.iter().any(|v| OP_AUTHORING.contains(&v.as_str())) {
                    ended.push((z, l));
                }
            }
        }
        // The compound's frontier is authority-ending too (D-196).
        self.consume_dead_stages(&ended);
        self.revoked_ids.push(rid);
        self.pending_compounds.remove(&oh);
        Ok(())
    }

    /// `c.revoke_device`, exclude mode — the D-180/D-186 compound. A
    /// valid-but-incomplete compound RESERVES its chain position
    /// (D-195: the control chain continues past a pending compound —
    /// pendency blocks only the compound's own effects; contrast
    /// D-112, where a FAILED op exerts no precedence) and re-evaluates
    /// toward completion as exclusions and cutoffs accumulate.
    fn admit_revoke_device(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        let oh = op.op_hash();
        if let Some(&rid) = self.pending_compounds.get(&oh) {
            // Reserved re-evaluation: the position is already held
            // and the bytes were validated at reservation — only the
            // completion question remains.
            let cutoffs = match Self::compound_cutoffs(&op.body)? {
                Ok(c) => c,
                Err(v) => return ok(Err(v)),
            };
            return ok(self.try_complete_compound(oh, rid, &cutoffs));
        }
        if let Err(v) = self.ctrl_admin_preamble(op) {
            return ok(Err(v));
        }
        let body = &op.body;
        match body.get("mode").and_then(|m| m.as_text()) {
            Some("exclude") => {}
            Some("compromise") => {
                return Err(Unimplemented("compromise mode (T4 receipt cutoffs)".into()))
            }
            _ => return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent"))),
        }
        if body.get("receipt_cutoffs").is_some() {
            return Err(Unimplemented("receipt_cutoffs under exclude".into()));
        }
        let Some(rid) = b16_field(body, "revocation_id") else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        // At most one live compound per revocation_id; a completed
        // target has no live certificate left to revoke.
        if self.pending_compounds.values().any(|r| *r == rid) || self.revoked_ids.contains(&rid) {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }
        // rotation_refs are typed linkage, never coverage — the
        // tranche mints none (legal: completion is state-derived).
        match body.get("rotation_refs").and_then(|r| r.as_array()) {
            None => return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent"))),
            Some(a) if !a.is_empty() => return Err(Unimplemented("rotation_refs linkage".into())),
            Some(_) => {}
        }
        // The target: every certificate bearing the revocation_id.
        let targets: Vec<[u8; 16]> = self
            .certs
            .iter()
            .filter(|c| c.revocation_id == rid)
            .map(|c| c.device_id)
            .collect();
        if targets.is_empty() {
            // Whether an unknown-target compound pends — and whether
            // it may reserve a position it could later fail
            // validation at — is unpinned; honest abort until a
            // vector decides it.
            return Err(Unimplemented("compound target not enrolled".into()));
        }
        // Cutoffs name the target's lineage exactly, in a known zone.
        let cutoffs = match Self::compound_cutoffs(body)? {
            Ok(c) => c,
            Err(v) => return ok(Err(v)),
        };
        for &(cz, cl) in &cutoffs {
            let names_target = self
                .lineages
                .iter()
                .any(|(l, d)| *l == cl && targets.contains(d));
            if !names_target || !self.zones.contains(&cz) {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            }
            // Empty heads are legal for a lineage with no accepted
            // ops (D-143) — the only shape the tranche mints.
            if self
                .tenant_chains
                .keys()
                .any(|(z, l, _)| *z == cz && *l == cl)
            {
                return Err(Unimplemented("cutoff heads below accepted ops".into()));
            }
        }
        // Reserve the position, then evaluate (the compound may
        // complete immediately).
        self.ctrl_next_seq += 1;
        self.ctrl_head = oh;
        self.pending_compounds.insert(oh, rid);
        ok(self.try_complete_compound(oh, rid, &cutoffs))
    }

    /// `c.cutoff`, requesterless pure-staging form only (D-136): an
    /// empty ratify set with non-empty `closes`, recorded INERT for a
    /// later consuming advance. The ratify machine (requester
    /// attestation, snapshot-wins, per-generation entries) is a later
    /// slice.
    fn admit_cutoff(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        if let Err(v) = self.ctrl_admin_preamble(op) {
            return ok(Err(v));
        }
        let body = &op.body;
        if body.get("requester").is_some() {
            return Err(Unimplemented("cutoff requester attestation".into()));
        }
        let Some(ratify) = body.get("cutoffs").and_then(|c| c.as_array()) else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        if !ratify.is_empty() {
            return Err(Unimplemented("ratify cutoffs".into()));
        }
        // "an operation with neither entries nor closes nor requester
        // is body-invariant".
        let closes = match body.get("closes").and_then(|c| c.as_array()) {
            Some(a) if !a.is_empty() => a,
            _ => return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent"))),
        };
        let mut staged: Vec<ZoneLineage> = Vec::new();
        for cn in closes {
            let (Some(z), Some(l)) = (b16_field(cn, "zone_id"), b16_field(cn, "lineage")) else {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            };
            match cn.get("heads").and_then(|h| h.as_array()) {
                None => return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent"))),
                Some(a) if !a.is_empty() => {
                    return Err(Unimplemented("frontierclose heads".into()))
                }
                Some(_) => staged.push((z, l)),
            }
        }

        // Accept: the stages exist from acceptance on (D-160), inert.
        self.staged_closes.extend(staged);
        self.ctrl_next_seq += 1;
        self.ctrl_head = op.op_hash();
        ok(Ok(()))
    }

    /// `c.cap_epoch_bump` — §9.4 consecutiveness plus the
    /// D-78/D-93/D-136/D-143/D-153 union-coverage rule under strict:
    /// this operation's entries ∪ the zone's UNCONSUMED stages must
    /// cover every live lineage; acceptance consumes every applicable
    /// stage one-shot (a dead stage was already vacuously spent at its
    /// authority-ending frontier and never counts, D-196).
    fn admit_cap_epoch_bump(
        &mut self,
        op: &SignedOp,
    ) -> Result<Result<(), Verdict>, Unimplemented> {
        if let Err(v) = self.ctrl_admin_preamble(op) {
            return ok(Err(v));
        }
        let body = &op.body;
        let Some(zone) = b16_field(body, "zone_id") else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        let Some(&cur) = self.cap_epochs.get(&zone) else {
            // The zone's creation may arrive later (interpretation
            // register #24 — unpinned).
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        if self.zone_strict.get(&zone) != Some(&true) {
            return Err(Unimplemented("non-strict zone coverage".into()));
        }
        if body.get("new_epoch").and_then(|n| n.as_uint()) != Some(cur + 1) {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }
        // Closure entries: each names THIS zone and a live lineage
        // (D-151); only the empty-heads shape is minted so far.
        let live = self.live_lineages(zone);
        let mut entries: Vec<[u8; 16]> = Vec::new();
        if let Some(cs) = body.get("cutoffs") {
            let Some(cs) = cs.as_array() else {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            };
            for cn in cs {
                let (Some(cz), Some(cl)) = (b16_field(cn, "zone_id"), b16_field(cn, "lineage"))
                else {
                    return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
                };
                if cz != zone || !live.contains(&cl) {
                    return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
                }
                match cn.get("heads").and_then(|h| h.as_array()) {
                    None => {
                        return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")))
                    }
                    Some(a) if !a.is_empty() => {
                        return Err(Unimplemented("frontierclose heads".into()))
                    }
                    Some(_) => entries.push(cl),
                }
            }
        }
        // Union coverage: entries ∪ unconsumed stages for this zone.
        let covered = |l: &[u8; 16]| {
            entries.contains(l)
                || self
                    .staged_closes
                    .iter()
                    .any(|&(sz, sl)| sz == zone && sl == *l)
        };
        if live.iter().any(|l| !covered(l)) {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }

        // Accept: advance the capability epoch; the consuming advance
        // spends EVERY unconsumed stage for this zone (D-153 one-shot
        // — a prior advance's materialized entries never satisfy
        // later coverage). Budget-window state (D-79) has no consumer
        // in the engine yet.
        self.cap_epochs.insert(zone, cur + 1);
        self.staged_closes.retain(|&(sz, _)| sz != zone);
        self.ctrl_next_seq += 1;
        self.ctrl_head = op.op_hash();
        ok(Ok(()))
    }

    /// `c.kek_rotate` — §5.5's admission face: dense per-zone epochs
    /// (every earlier control op is already folded at this chain
    /// position, so consecutiveness is a plain body invariant),
    /// validated wraps at the new epoch, and the D-81 last-holder
    /// floor (≥ 1 recipient — the CDDL's `[+ kekwrap]`). The Fence/
    /// rewrap/destroy states are local storage, not admission.
    fn admit_kek_rotate(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        if let Err(v) = self.ctrl_admin_preamble(op) {
            return ok(Err(v));
        }
        let body = &op.body;
        let Some(zone) = b16_field(body, "zone_id") else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        let Some(&cur) = self.kek_epochs.get(&zone) else {
            // The zone's creation may arrive later (interpretation
            // register #24 — unpinned).
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        if body.get("new_epoch").and_then(|n| n.as_uint()) != Some(cur + 1) {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }
        match body.get("erase_manifest").and_then(|m| m.as_array()) {
            None => return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent"))),
            Some(a) if !a.is_empty() => return Err(Unimplemented("erase manifest".into())),
            Some(_) => {}
        }
        let plane = self
            .plane_id
            .expect("admin key resolved ⇒ genesis installed");
        let wraps = match body.get("wraps").and_then(|w| w.as_array()) {
            Some(a) if !a.is_empty() => a,
            _ => return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent"))),
        };
        let mut recipients: Vec<[u8; 16]> = Vec::new();
        for wn in wraps {
            match self.check_wrap(wn, plane, zone, cur + 1, None)? {
                Ok(r) => {
                    if recipients.contains(&r) {
                        // Duplicate set key (zone, epoch, device).
                        return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
                    }
                    recipients.push(r);
                }
                Err(v) => return ok(Err(v)),
            }
        }

        // Accept: the new epoch's recipient set IS the wrap set.
        // Pending compounds re-evaluate through the fold's fixpoint.
        self.kek_epochs.insert(zone, cur + 1);
        for r in recipients {
            self.record_wrap(zone, cur + 1, r);
        }
        self.ctrl_next_seq += 1;
        self.ctrl_head = op.op_hash();
        ok(Ok(()))
    }

    /// Tenant `m.claim` under the dev arm (D-199: unheld citations
    /// pend).
    fn admit_claim(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        let h = &op.header;
        if h.tenant != "memory" {
            return Err(Unimplemented(format!("tenant {}", h.tenant)));
        }
        let Proof::Dev { cert, cap } = h.proof else {
            return ok(Err(Verdict::Rejected("proof-arm", "reject-permanent")));
        };
        // Resolve citations by hash — a cited certificate or grant
        // not yet held is `ref-unresolved`, indefinitely if need be
        // (D-199; D-194's absence proof is withdrawn).
        let Some(held_cert) = self.certs.iter().find(|c| c.h_cert == cert).cloned() else {
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        let Some(grant) = self.grants.iter().find(|g| g.h_grant == cap).cloned() else {
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        // Post-revocation claims need D-86 position-relative validity
        // (the signed-before-the-boundary prefix stands) — a later
        // slice; no tranche fixture claims across a revocation.
        if held_cert.revoked {
            return Err(Unimplemented("claim under a revoked certificate".into()));
        }
        if grant.revoked {
            return Err(Unimplemented("claim under a revoked grant".into()));
        }

        // Signature under the resolved certificate key.
        if h.signer_alg != "ed25519" {
            return Err(Unimplemented("p256 tenant signer".into()));
        }
        if !op.verify_ed25519(&held_cert.sig_pk)
            || h.signer_key_id != domains::key_id("ed25519", &held_cert.sig_pk)
        {
            return ok(Err(Verdict::Rejected("sig-invalid", "reject-permanent")));
        }
        if !op.body_hash_ok() {
            return ok(Err(Verdict::Rejected("body-hash", "reject-permanent")));
        }

        // O8: the daemon/human/browser/service actor id is the hex
        // device id.
        let want_id: String = held_cert
            .device_id
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if ["human", "daemon", "browser", "service"].contains(&h.actor_kind)
            && h.actor_id != want_id
        {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }

        // Proof stage: grant scope (tenant ∧ zone ∧ space ∧ op ∧
        // kind), lineage binding.
        if grant.subject_device != held_cert.device_id {
            return ok(Err(Verdict::Rejected("no-grant", "reject-permanent")));
        }
        if !grant.tenants.iter().any(|t| t == "memory") {
            return ok(Err(Verdict::Rejected("scope-tenant", "reject-permanent")));
        }
        if let Some(z) = grant.zone {
            if z != h.zone_id {
                return ok(Err(Verdict::Rejected("scope-zone", "reject-permanent")));
            }
        }
        if let Some(spaces) = &grant.spaces {
            if !spaces.contains(&h.space_id) {
                return ok(Err(Verdict::Rejected("scope-space", "reject-permanent")));
            }
        }
        let verb = match h.operation_type {
            "m.claim" => "propose",
            other => return Err(Unimplemented(format!("op_type {other}"))),
        };
        if !grant.verbs.iter().any(|v| v == verb) {
            return ok(Err(Verdict::Rejected("scope-op", "reject-permanent")));
        }
        if grant.lineage != Some(h.writer_lineage) {
            return ok(Err(Verdict::Rejected("no-grant", "reject-permanent")));
        }
        if let Some(kinds) = &grant.kinds {
            let kind = op.body.get("kind").and_then(|k| k.as_text()).unwrap_or("");
            if !kinds.iter().any(|k| k == kind) {
                return ok(Err(Verdict::Rejected("scope-kind", "reject-permanent")));
            }
        }

        // Chain: within (zone, lineage, gen), dense from 1.
        let key = (h.zone_id, h.writer_lineage, h.writer_gen);
        let (expect_seq, head) = self
            .tenant_chains
            .get(&key)
            .copied()
            .unwrap_or((1, [0u8; 32]));
        if h.writer_gen != 1 {
            return Err(Unimplemented("w.gen generations".into()));
        }
        match h.writer_sequence.cmp(&expect_seq) {
            std::cmp::Ordering::Less => return ok(Err(Verdict::Rejected("fork", "freeze-writer"))),
            std::cmp::Ordering::Greater => {
                return ok(Err(Verdict::Pending(
                    "causal-missing",
                    "pending-dependency",
                )))
            }
            std::cmp::Ordering::Equal => {}
        }
        let want_prev = if expect_seq == 1 {
            domains::gen_start(&h.writer_lineage, 1)
        } else {
            head
        };
        if h.previous_writer_hash != want_prev {
            return ok(Err(Verdict::Rejected("fork", "freeze-writer")));
        }

        // Epochs: capability_epoch 1 is open at genesis/zone-create;
        // grant slack lower bound.
        if h.capability_epoch != 1 || h.authored_kek_epoch != 1 {
            return Err(Unimplemented("non-initial epochs".into()));
        }
        if grant.capability_epoch > h.capability_epoch {
            return ok(Err(Verdict::Rejected(
                "capability-epoch",
                "quarantine-reproposal",
            )));
        }

        // Accept.
        self.tenant_chains
            .insert(key, (h.writer_sequence + 1, op.op_hash()));
        ok(Ok(()))
    }

    /// Dispatch one operation.
    fn admit(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        match op.header.operation_type {
            "c.genesis" => self.admit_genesis(op),
            "c.enroll" => self.admit_enroll(op),
            "c.grant" => self.admit_grant(op),
            "c.revoke_grant" => self.admit_revoke_grant(op),
            "c.revoke_device" => self.admit_revoke_device(op),
            "c.cutoff" => self.admit_cutoff(op),
            "c.cap_epoch_bump" => self.admit_cap_epoch_bump(op),
            "c.kek_rotate" => self.admit_kek_rotate(op),
            "m.claim" => self.admit_claim(op),
            other => Err(Unimplemented(format!("op_type {other}"))),
        }
    }
}

/// One fold run over a delivery order. Returns the per-item verdict
/// history: `snapshots[i]` = every item's verdict immediately after
/// delivery position `i` folded (for trace evaluation), plus the
/// final map.
pub struct Run {
    pub final_verdicts: BTreeMap<String, Verdict>,
    pub snapshots: Vec<BTreeMap<String, Verdict>>,
}

pub fn run_delivery(
    items: &BTreeMap<String, Vec<u8>>,
    order: &[String],
) -> Result<Run, Unimplemented> {
    let mut state = State::default();
    let mut verdicts: BTreeMap<String, Verdict> = BTreeMap::new();
    let mut snapshots = Vec::new();
    // Pending queue in arrival order.
    let mut pending: Vec<String> = Vec::new();

    for name in order {
        let bytes = &items[name];
        let verdict = classify(&mut state, bytes)?;
        verdicts.insert(name.clone(), verdict);
        if matches!(verdict, Verdict::Pending(..)) {
            pending.push(name.clone());
        }
        // Re-evaluate the pending set to fixpoint after any
        // acceptance (arrival order preserved).
        loop {
            let mut progressed = false;
            let mut still_pending = Vec::new();
            for pname in pending.drain(..) {
                let v = classify(&mut state, &items[&pname])?;
                verdicts.insert(pname.clone(), v);
                match v {
                    Verdict::Pending(..) => still_pending.push(pname),
                    Verdict::Admitted => progressed = true,
                    Verdict::Rejected(..) => {}
                }
            }
            pending = still_pending;
            if !progressed {
                break;
            }
        }
        snapshots.push(verdicts.clone());
    }
    Ok(Run {
        final_verdicts: verdicts,
        snapshots,
    })
}

fn classify(state: &mut State, bytes: &[u8]) -> Result<Verdict, Unimplemented> {
    let op = match parse_op(bytes) {
        Ok(op) => op,
        Err(crate::envelope::OpError::Parse(e)) => {
            use crate::cbor::DecodeError as D;
            let outcome = match e {
                D::Depth => "depth",
                D::NonCanonical | D::UintRange => "non-canonical",
                D::Malformed | D::TrailingBytes => "malformed",
            };
            return Ok(Verdict::Rejected(outcome, "reject-permanent"));
        }
        Err(crate::envelope::OpError::Version) => {
            return Ok(Verdict::Rejected("unknown-version", "reject-permanent"));
        }
        Err(crate::envelope::OpError::Shape(_)) => {
            return Ok(Verdict::Rejected("malformed", "reject-permanent"));
        }
    };
    state.admit(&op).map(|r| match r {
        Ok(()) => Verdict::Admitted,
        Err(v) => v,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(name: &str) -> (BTreeMap<String, Vec<u8>>, serde_json::Value) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vectors")
            .join(name);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut items = BTreeMap::new();
        for (k, hv) in v["inputs"]["items"].as_object().unwrap() {
            let s = hv.as_str().unwrap();
            items.insert(
                k.clone(),
                (0..s.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                    .collect(),
            );
        }
        (items, v)
    }

    #[test]
    fn negation_residual_folds_all_admitted() {
        let (items, _) = load("f07-negation-residual-acceptance.json");
        let run = run_delivery(&items, &["c1".into(), "c2".into()]).unwrap();
        assert_eq!(run.final_verdicts["c1"], Verdict::Admitted);
        assert_eq!(run.final_verdicts["c2"], Verdict::Admitted);
    }

    #[test]
    fn delayed_reference_converges_with_intermediate_pend() {
        let (items, _) = load("f07-delayed-reference-convergence-c1-i-c2.json");
        // Order 1: C1 → I → C2 — I pends after its own delivery.
        let run = run_delivery(&items, &["c1".into(), "i".into(), "c2".into()]).unwrap();
        assert_eq!(
            run.snapshots[1]["i"],
            Verdict::Pending("ref-unresolved", "pending-dependency")
        );
        assert_eq!(run.final_verdicts["i"], Verdict::Admitted);
        // Order 2: C1 → C2 → I — admits immediately.
        let run2 = run_delivery(&items, &["c1".into(), "c2".into(), "i".into()]).unwrap();
        assert_eq!(run2.final_verdicts, run.final_verdicts);
    }

    /// The D-195 story: the compound pends `ref-unresolved` while the
    /// wrap domain is nonempty, HOLDS its chain position (g and k
    /// admit past it), the window grant admits, and the completing
    /// rotation flips the compound at fixpoint. Both delivery orders
    /// converge.
    #[test]
    fn pending_revocation_reserves_and_completes_at_the_rotation() {
        let (items, _) = load("f07-pending-revocation-window-grant-completing-rotation.json");
        let all = ["c1", "c2", "r", "g", "k"];

        let o1: Vec<String> = all.iter().map(|s| s.to_string()).collect();
        let run = run_delivery(&items, &o1).unwrap();
        assert_eq!(
            run.snapshots[2]["r"],
            Verdict::Pending("ref-unresolved", "pending-dependency")
        );
        // The window grant admits while the compound pends (its
        // previous_writer_hash cites the RESERVED position's op).
        assert_eq!(run.snapshots[3]["g"], Verdict::Admitted);
        assert_eq!(
            run.snapshots[3]["r"].pair(),
            Some(("ref-unresolved", "pending-dependency"))
        );
        for k in all {
            assert_eq!(run.final_verdicts[k], Verdict::Admitted, "{k}");
        }

        // Order 2: g and k pend causal-missing below the compound's
        // unfilled seq; r's arrival fills it and cascades.
        let o2: Vec<String> = ["c1", "c2", "g", "k", "r"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let run2 = run_delivery(&items, &o2).unwrap();
        assert_eq!(
            run2.snapshots[2]["g"],
            Verdict::Pending("causal-missing", "pending-dependency")
        );
        assert_eq!(
            run2.snapshots[3]["k"],
            Verdict::Pending("causal-missing", "pending-dependency")
        );
        assert_eq!(run2.final_verdicts, run.final_verdicts);
    }

    /// D-153/D-196: the staged close dies vacuously at the
    /// authority-ending revocation, so after the regrant the dev1-only
    /// bump lacks fresh coverage and rejects — and its corrected
    /// successor legally reuses the position (D-112: a failed op
    /// exerts no precedence).
    #[test]
    fn dead_stage_never_counts_and_rejected_candidate_frees_its_position() {
        let (items, _) = load("f07-staged-frontier-consumed-no-resurrection.json");
        let order: Vec<String> = ["c1", "c2", "s", "rg", "g4", "k1", "k2"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let run = run_delivery(&items, &order).unwrap();
        assert_eq!(
            run.final_verdicts["k1"],
            Verdict::Rejected("body-invariant", "reject-permanent")
        );
        for k in ["c1", "c2", "s", "rg", "g4", "k2"] {
            assert_eq!(run.final_verdicts[k], Verdict::Admitted, "{k}");
        }
    }
}
