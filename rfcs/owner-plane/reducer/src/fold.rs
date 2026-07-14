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
//! acceptance side), and `m.claim` under the dev arm (D-199:
//! unresolved certificate/grant citations pend `ref-unresolved` and
//! admit on arrival).

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
    // Consumed by the revocation slice (the next engine layer).
    #[allow(dead_code)]
    revocation_id: [u8; 16],
}

#[derive(Debug, Clone)]
struct HeldGrant {
    h_grant: [u8; 32],
    subject_device: [u8; 16],
    lineage: Option<[u8; 16]>,
    zone: Option<[u8; 16]>,
    spaces: Option<Vec<[u8; 16]>>,
    verbs: Vec<String>,
    tenants: Vec<String>,
    kinds: Option<Vec<String>>,
    capability_epoch: u64,
    imports: bool,
}

/// (zone, lineage, gen) — one tenant chain's coordinates.
type ChainKey = ([u8; 16], [u8; 16], u64);
/// (next expected seq, current head op hash).
type ChainHead = (u64, [u8; 32]);

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
            revocation_id: b16_field(cert_node, "revocation_id").unwrap_or_default(),
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
        });
        ok(())
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

        // Accept: install the plane.
        self.plane_id = Some(op.header.plane_id);
        self.root_pk = Some(root_pk);
        self.zones.push(zone_id.unwrap_or_default());
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

    /// `c.enroll`, new-device shape (`cert.renews` absent).
    fn admit_enroll(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        if let Err(v) = Self::ctrl_header_pins(op) {
            return ok(Err(v));
        }
        if let Err(v) = self.ctrl_chain(op) {
            return ok(Err(v));
        }
        let Proof::Admin { epoch, .. } = op.header.proof else {
            return ok(Err(Verdict::Rejected("proof-arm", "reject-permanent")));
        };
        let admin_pk = match self.admin_key(epoch) {
            Ok(pk) => pk,
            Err(v) => return ok(Err(v)),
        };
        if !op.verify_ed25519(&admin_pk)
            || op.header.signer_key_id != domains::key_id("ed25519", &admin_pk)
        {
            return ok(Err(Verdict::Rejected("sig-invalid", "reject-permanent")));
        }
        if !op.body_hash_ok() {
            return ok(Err(Verdict::Rejected("body-hash", "reject-permanent")));
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

        // Grants: every entry targets the enrolled device; a second
        // active import-verb grant for a destination zone rejects
        // (D-139/D-146).
        let mut new_grants = Vec::new();
        if let Some(grants) = body.get("grants").and_then(|g| g.as_array()) {
            for gn in grants {
                if b16_field(gn, "subject_device") != Some(device_id) {
                    return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
                }
                let has_import = gn
                    .get("ops")
                    .and_then(|o| o.as_array())
                    .is_some_and(|a| a.iter().any(|v| v.as_text() == Some("import")));
                if has_import {
                    let gzone = gn.get("zone").and_then(|z| z.bytes_n::<16>());
                    if self.grants.iter().any(|g| g.imports && g.zone == gzone) {
                        return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
                    }
                }
                new_grants.push(gn.clone());
            }
        }

        // Accept.
        self.lineages.push((lineage_id, device_id));
        self.record_cert(cert)?;
        for gn in &new_grants {
            self.record_grant(gn)?;
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

    /// Unimplemented op types abort honestly.
    #[test]
    fn revocation_fixture_reports_unimplemented() {
        let (items, v) = load("f07-pending-revocation-window-grant-completing-rotation.json");
        let order: Vec<String> = v["inputs"]["deliveries"][0]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect();
        assert!(run_delivery(&items, &order).is_err());
    }
}
