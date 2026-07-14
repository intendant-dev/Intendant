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
//!
//! The import arc (§11.8): `c.zone_create`/`c.space_create`,
//! `m.export.release` (flow matching, class-floor law, held-claim
//! sources), `m.import.claim` (per-record Merkle proof, live-source
//! equality, the D-134 derived shape), the derived claimant fold
//! (D-155 total order; freeze via the authority-ending frontier;
//! D-161/D-169 collision), and `c.recovery_succession` at the head
//! (named preserves + the revivable omission blanket, D-132/D-151).
//! Held tenant classifications are DERIVED (§10.5): `run_delivery`
//! overlays them after every fixpoint, so a later boundary
//! retro-quarantines and a dead basis re-derives ownership.

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
    /// §9.1: a certificate deadline binds like a grant deadline.
    expiry_deadline_ms: Option<u64>,
    /// Set at a `c.revoke_device` compound's COMPLETING acceptance
    /// (D-195 — a pending compound ends nothing yet).
    revoked: bool,
}

/// One grant `flows` entry (§4.3): the release-matching facts.
#[derive(Debug, Clone)]
struct FlowFacts {
    from_zone: [u8; 16],
    from_space: Option<[u8; 16]>,
    /// The endpoint's canonical bytes — matching is byte equality.
    to_raw: Vec<u8>,
    kinds: Option<Vec<String>>,
    class_ceiling: u8,
    expiry_deadline_ms: u64,
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
    /// The control position that accepted this grant — the claimant
    /// order's major key (D-155).
    ctrl_pos: u64,
    class_ceiling: Option<u8>,
    flows: Vec<FlowFacts>,
    /// §9.1: a present deadline always binds, in every posture.
    expiry_deadline_ms: Option<u64>,
    /// T5: both lease legs required when set.
    online_lease: bool,
    max_age_ms: Option<u64>,
    /// `c.revoke_grant`, or derived revocation at a device compound's
    /// completion (D-85).
    revoked: bool,
    /// The revocation cutoff's carried `(gen, seq)` heads — an
    /// at-or-below preserved claimant is thereby FROZEN (D-155).
    revoke_caps: Option<Vec<(u64, u64)>>,
}

/// One accepted (held) tenant operation. Its FOLD verdict is derived
/// (§10.5): boundaries and claimant folds re-classify held ops on
/// every state change — acceptance into the chain is permanent, the
/// classification is not.
#[derive(Debug, Clone)]
struct HeldTenantOp {
    op_hash: [u8; 32],
    zone: [u8; 16],
    space: [u8; 16],
    lineage: [u8; 16],
    gen: u64,
    seq: u64,
    cited_grant: [u8; 32],
    /// The signed actor (kind, id) — the §11.2 authoring principal is
    /// (lineage, kind, id).
    actor_kind: String,
    actor_id: String,
    /// §10.1 shape-1 direct-human evidence: human kind, no
    /// attestation (class-compatibility rides the §7.6 table; the
    /// tranche's classes all admit human presence).
    human_evidence: bool,
    /// §11.4 derived actor class.
    actor_class: &'static str,
    /// m.claim content — (kind, statement, sensitivity rank): the
    /// D-134 source-equality material.
    claim: Option<(String, String, u8)>,
    release: Option<ReleaseFacts>,
    import: Option<ImportFacts>,
    judge: Option<JudgeFacts>,
}

/// One admitted judgment's §11.2 counting facts.
#[derive(Debug, Clone)]
struct JudgeFacts {
    verdict: String,
    target: [u8; 32],
    /// Supersede only.
    replacement: Option<[u8; 32]>,
}

#[derive(Debug, Clone)]
struct ReleaseFacts {
    export_id: [u8; 16],
    sources: Vec<[u8; 32]>,
    content_digest: [u8; 32],
    dest_zone: [u8; 16],
    dest_space: [u8; 16],
}

#[derive(Debug, Clone)]
struct ImportFacts {
    /// The replay key: `(from_plane, release_op, source_op)` (D-123).
    key: ([u8; 32], [u8; 32], [u8; 32]),
    /// The citing import grant's control position — the claimant
    /// order's major key (D-155).
    grant_pos: u64,
}

/// A tenant-history boundary: operations of `(zone, lineage)`
/// at-or-below a cap stand; beyond them — or in uncarried
/// generations — `(cutoff, quarantine-reproposal)`.
#[derive(Debug, Clone)]
struct TenantBoundary {
    zone: [u8; 16],
    lineage: [u8; 16],
    /// Revoke boundaries select the revoked grant's operations only;
    /// recover-purpose entries are global selectors (D-143).
    selector_grant: Option<[u8; 32]>,
    /// (gen, max seq) pairs — empty = nothing stands.
    caps: Vec<(u64, u64)>,
}

/// One registered space: id, zone, class, and its bound status
/// policy (id + carried hash — the polref every judgment must match).
#[derive(Debug, Clone)]
struct SpaceInfo {
    space_id: [u8; 16],
    /// Recorded for the space→zone consistency checks later slices
    /// add (space-retire, cross-zone scope).
    #[allow(dead_code)]
    zone_id: [u8; 16],
    space_class: String,
    policy_id: String,
    policy_hash: [u8; 32],
}

/// One accepted C3′: named entries preserve at-or-below (immutable
/// termination); every omitted `(zone, lineage)` whose lineage was
/// enrolled at or before base folds the revivable `"none"` override
/// (D-132/D-138/D-151).
#[derive(Debug, Clone)]
struct RecoveryState {
    named: Vec<TenantBoundary>,
    lineages_at_base: Vec<[u8; 16]>,
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

/// §7.5 (b): the hosted-grantable set (the safe verbs plus the
/// system-only `audit.write`).
const HOSTED_GRANTABLE: &[&str] = &[
    "search",
    "read",
    "evidence.read",
    "propose",
    "assert",
    "judge.safe",
    "pin.safe",
    "erase.request",
    "raise",
    "audit.write",
];

/// §7.5 (c): control operations admissible under the hosted ceiling
/// (the deeper per-op constraints — exclusion shape, own-lineage,
/// ratify-only — are separate rules).
const HOSTED_CTRL_ADMISSIBLE: &[&str] = &[
    "c.enroll",
    "c.revoke_device",
    "c.kek_rotate",
    "c.wrap_add",
    "c.lineage_reauth",
    "c.cutoff",
    "c.abandon_writer",
    "c.drill",
    "c.recovery_succession",
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
/// An O5 replay-registry key: (zone, lineage, request_id).
type ReplayKey = ([u8; 16], [u8; 16], [u8; 16]);
/// A held release's re-derivation facts:
/// (op_hash, export_id, content_digest, sources).
type ReleaseView = ([u8; 32], [u8; 16], [u8; 32], Vec<[u8; 32]>);

/// Derived plane state — grown only by ACCEPTED operations.
#[derive(Debug, Clone, Default)]
pub struct State {
    plane_id: Option<[u8; 32]>,
    root_pk: Option<[u8; 32]>,
    ctrl_next_seq: u64,
    ctrl_head: [u8; 32],
    zones: Vec<[u8; 16]>,
    spaces: Vec<SpaceInfo>,
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
    /// Accepted tenant operations — the derived lanes re-classify
    /// these against boundaries and claimant folds (§10.5).
    held_tenant: Vec<HeldTenantOp>,
    /// Immutable tenant boundaries (revoke cutoffs so far).
    tenant_boundaries: Vec<TenantBoundary>,
    /// Accepted C3′ recoveries (named preserves + omission blankets).
    recoveries: Vec<RecoveryState>,
    /// epoch → admin key (epoch 1 = the root key; successions and
    /// C3′ install later epochs).
    admin_keys: BTreeMap<u64, [u8; 32]>,
    /// Recovery epoch (0 at genesis; C3′ = current + 1).
    repoch: u64,
    /// The current recovery commitment (`H_drill(recovery_pk)`).
    recovery_commitment: Option<[u8; 32]>,
    /// O5 replay registry, scoped to the writer (§11.1): (zone,
    /// lineage, request_id) → accepted op hash. Byte-identical
    /// redelivery = `duplicate` (idempotent); differing bytes under a
    /// consumed request_id = `request-fork`. Only ACCEPTANCE consumes
    /// (a failed op exerts no precedence, D-112).
    request_seen: BTreeMap<ReplayKey, [u8; 32]>,
    /// §5.4 erase queue: accepted `m.erase_request` targets (claim op
    /// hashes, acceptance order) — persisted until manifested.
    erase_queue: Vec<[u8; 32]>,
    /// §11.1: targets flagged retrieval-excluded IMMEDIATELY at the
    /// erase request's acceptance.
    retrieval_excluded: Vec<[u8; 32]>,
    /// (zone, capability epoch) → the policy-in-force's
    /// `time_witnesses` devices (the T2 anchor, D-69). Absent epochs
    /// resolve DOWNWARD — a bare bump carries policy(e−1) forward.
    policies: BTreeMap<([u8; 16], u64), Vec<[u8; 16]>>,
    /// Held `accept` receipts (aux, §4.7) — validated lazily at
    /// admission: qualification is a pure function of (receipt
    /// bytes, operation bytes, control history).
    receipts: Vec<AuxReceipt>,
    /// Held leases (aux, §4.7/T5).
    leases: Vec<AuxLease>,
    /// The §5.6 local index (aux `index`): item_addr → op hash.
    item_index: BTreeMap<[u8; 32], [u8; 32]>,
    /// The genesis descriptor's provenance ("trusted"/"hosted") —
    /// the §7.5 ceiling binds while hosted AND no recovery has been
    /// accepted (the lift is portable, D-42).
    provenance: Option<String>,
    /// The ACCEPTED control chain's exact bytes as (seq, bytes) —
    /// the D-138 total re-fold replays a surviving prefix from here.
    /// (A pending compound RESERVES its position without entering
    /// the log; completion pushes it when the re-evaluation accepts,
    /// so entry order may lag seq order — replay sorts by seq.)
    ctrl_log: Vec<(u64, Vec<u8>)>,
    /// Derived verdicts for control ops re-classified by a C2 freeze
    /// or a C3′ cut — overlaid like the tenant lane.
    ctrl_overlay: BTreeMap<[u8; 32], Verdict>,
    /// Hashes cut from the chain, by seq — a late redelivery of a
    /// cut-branch op classifies from here.
    cut_chain: BTreeMap<u64, Vec<[u8; 32]>>,
    /// C2: the control plane is frozen (no further control ops on
    /// either branch; only recovery resolves).
    ctrl_frozen: bool,
    /// The last accepted recovery's placement seq — a differing op
    /// arriving AT that position is cut-branch material, never C2
    /// (the §7.4 precedence exception).
    recovery_placement: Option<u64>,
}

/// A held Signed `accept` receipt from aux — unvalidated until an
/// operation cites time evidence.
#[derive(Debug, Clone)]
struct AuxReceipt {
    issuer_cert: [u8; 32],
    plane: [u8; 32],
    zone: [u8; 16],
    subject: [u8; 32],
    seen_ms: u64,
    stmt_raw: Vec<u8>,
    sig: Vec<u8>,
}

/// A held Signed lease from aux (§4.7/T5).
#[derive(Debug, Clone)]
struct AuxLease {
    issuer_cert: [u8; 32],
    plane: [u8; 32],
    zone: [u8; 16],
    grant_id: [u8; 16],
    lineage: [u8; 16],
    issued_ms: u64,
    expires_ms: u64,
    stmt_raw: Vec<u8>,
    sig: Vec<u8>,
}

/// Verify a `Signed<…>` statement: sig over `msg(tag, stmt bytes)`.
fn verify_stmt(pk: &[u8; 32], tag: &str, stmt_raw: &[u8], sig: &[u8]) -> bool {
    use ed25519_dalek::{Signature, VerifyingKey};
    let Ok(vk) = VerifyingKey::from_bytes(pk) else {
        return false;
    };
    let Ok(sig_arr) = <[u8; 64]>::try_from(sig) else {
        return false;
    };
    vk.verify_strict(
        &domains::msg(tag, stmt_raw),
        &Signature::from_bytes(&sig_arr),
    )
    .is_ok()
}

/// A zone policy's `time_witnesses` as device ids (`"connect"` has
/// no engine yet).
fn policy_witness_devices(policy: Option<&Node>) -> Result<Vec<[u8; 16]>, Unimplemented> {
    let mut out = Vec::new();
    if let Some(ws) = policy
        .and_then(|p| p.get("time_witnesses"))
        .and_then(|w| w.as_array())
    {
        for w in ws {
            if let Some(d) = w.bytes_n::<16>() {
                out.push(d);
            } else if w.as_text() == Some("connect") {
                return Err(Unimplemented("connect time witness".into()));
            }
        }
    }
    Ok(out)
}

/// §11.6 classification lattice rank.
fn class_rank(c: &str) -> Option<u8> {
    match c {
        "public" => Some(0),
        "internal" => Some(1),
        "private" => Some(2),
        "sensitive" => Some(3),
        _ => None,
    }
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

    /// Admin-arm resolution: epoch 1 is the root key; successions and
    /// C3′ install later epochs. Pre-genesis, every arm pends.
    fn admin_key(&self, epoch: u64) -> Result<[u8; 32], Verdict> {
        if self.admin_keys.is_empty() {
            return Err(Verdict::Pending("ref-unresolved", "pending-dependency"));
        }
        self.admin_keys
            .get(&epoch)
            .copied()
            .ok_or(Verdict::Rejected("proof-arm", "reject-permanent"))
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
            expiry_deadline_ms: cert_node
                .get("expiry_deadline_ms")
                .and_then(|n| n.as_uint()),
            revoked: false,
        });
        ok(())
    }

    fn record_grant(&mut self, grant_node: &Node, ctrl_pos: u64) -> Result<(), Unimplemented> {
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
        let mut flows = Vec::new();
        for fnode in grant_node
            .get("flows")
            .and_then(|f| f.as_array())
            .unwrap_or(&[])
        {
            flows.push(FlowFacts {
                from_zone: b16_field(fnode, "from_zone").unwrap_or_default(),
                from_space: b16_field(fnode, "from_space"),
                to_raw: fnode.get("to").map(|t| t.raw.to_vec()).unwrap_or_default(),
                kinds: fnode.get("kinds").and_then(|k| {
                    k.as_array().map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_text().map(str::to_string))
                            .collect()
                    })
                }),
                class_ceiling: fnode
                    .get("class_ceiling")
                    .and_then(|c| c.as_text())
                    .and_then(class_rank)
                    .unwrap_or(0),
                expiry_deadline_ms: fnode
                    .get("expiry_deadline_ms")
                    .and_then(|n| n.as_uint())
                    .unwrap_or(0),
            });
        }
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
            ctrl_pos,
            class_ceiling: grant_node
                .get("class_ceiling")
                .and_then(|c| c.as_text())
                .and_then(class_rank),
            flows,
            expiry_deadline_ms: grant_node
                .get("expiry_deadline_ms")
                .and_then(|n| n.as_uint()),
            online_lease: grant_node.get("online_lease").and_then(|n| n.as_bool()) == Some(true),
            max_age_ms: grant_node.get("max_age_ms").and_then(|n| n.as_uint()),
            revoked: false,
            revoke_caps: None,
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
        // §7.5 (b): under the hosted ceiling only the safe set plus
        // the system-only audit.write is grantable.
        if self.hosted_ceiling_active() && verbs.iter().any(|v| !HOSTED_GRANTABLE.contains(v)) {
            return Some(Verdict::Rejected("hosted-ceiling", "reject-permanent"));
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

        // Accept: install the plane. KEK epoch 1, capability epoch 1,
        // admin epoch 1 (the root key), repoch 0, and the recovery
        // commitment open here (§7.1 row); the B.1 policy's
        // strictness scopes the union-coverage rule.
        self.plane_id = Some(plane);
        self.root_pk = Some(root_pk);
        self.admin_keys.insert(1, root_pk);
        self.repoch = 0;
        self.recovery_commitment = descriptor
            .get("recovery_commitment")
            .and_then(|n| n.bytes_n::<32>());
        self.provenance = descriptor
            .get("provenance")
            .and_then(|n| n.as_text())
            .map(str::to_string);
        self.zones.push(zone_id.unwrap_or_default());
        self.kek_epochs.insert(zone_id.unwrap_or_default(), 1);
        self.cap_epochs.insert(zone_id.unwrap_or_default(), 1);
        let strict = body
            .get("zone_policy")
            .and_then(|p| p.get("strictness"))
            .and_then(|s| s.as_text())
            == Some("strict");
        self.zone_strict.insert(zone_id.unwrap_or_default(), strict);
        self.policies.insert(
            (zone_id.unwrap_or_default(), 1),
            policy_witness_devices(body.get("zone_policy"))?,
        );
        for r in recipients {
            self.record_wrap(zone_id.unwrap_or_default(), 1, r);
        }
        for sd in [home, audit] {
            if let Some(v) = self.record_space(sd, zone_id.unwrap_or_default())? {
                return ok(Err(v));
            }
        }
        self.lineages
            .push((lineage_id.unwrap(), device_id.unwrap()));
        self.record_cert(cert)?;
        for g in ["grant", "audit_grant"] {
            if let Some(gn) = body.get(g) {
                self.record_grant(gn, 1)?;
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
            self.record_grant(gn, op.header.writer_sequence)?;
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
        self.record_grant(gn, op.header.writer_sequence)?;
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
        let mut caps: Vec<(u64, u64)> = Vec::new();
        if op_authoring {
            let Some(cn) = cutoff else {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            };
            if b16_field(cn, "zone_id") != self.grants[idx].zone
                || b16_field(cn, "lineage") != self.grants[idx].lineage
            {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            }
            match self.parse_heads(
                cn,
                self.grants[idx].zone.unwrap_or_default(),
                self.grants[idx].lineage.unwrap_or_default(),
            )? {
                Ok(h) => caps = h,
                Err(v) => return ok(Err(v)),
            }
        } else if cutoff.is_some() {
            return Err(Unimplemented(
                "cutoff on a read-only grant revocation".into(),
            ));
        }

        // Accept: deactivate the grant, install the revoke boundary
        // (selector = the revoked grant — its operations at or below
        // the carried heads stand, beyond them quarantine), and mark
        // the freeze frontier (an at-or-below preserved claimant is
        // thereby frozen, D-155).
        let ended = match (self.grants[idx].zone, self.grants[idx].lineage) {
            (Some(z), Some(l)) if op_authoring => vec![(z, l)],
            _ => vec![],
        };
        self.grants[idx].revoked = true;
        if op_authoring {
            self.grants[idx].revoke_caps = Some(caps.clone());
            self.tenant_boundaries.push(TenantBoundary {
                zone: self.grants[idx].zone.unwrap_or_default(),
                lineage: self.grants[idx].lineage.unwrap_or_default(),
                selector_grant: Some(self.grants[idx].h_grant),
                caps,
            });
        }
        self.consume_dead_stages(&ended);
        self.ctrl_next_seq += 1;
        self.ctrl_head = op.op_hash();
        ok(Ok(()))
    }

    /// Parse a frontierclose's `heads` into `(gen, seq)` caps,
    /// resolving each against the HELD chain: the named coordinate
    /// must hold exactly the named op (an unheld head pends,
    /// `ref-unresolved` — the c.cutoff row's rule; a held-but-
    /// different op is unpinned).
    fn parse_heads(
        &self,
        cn: &Node,
        zone: [u8; 16],
        lineage: [u8; 16],
    ) -> Result<Result<Vec<(u64, u64)>, Verdict>, Unimplemented> {
        let Some(heads) = cn.get("heads").and_then(|h| h.as_array()) else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        let mut caps = Vec::new();
        for hn in heads {
            let (Some(hl), Some(gen), Some(seq), Some(hop)) = (
                b16_field(hn, "lineage"),
                hn.get("gen").and_then(|n| n.as_uint()),
                hn.get("seq").and_then(|n| n.as_uint()),
                hn.get("op").and_then(|n| n.bytes_n::<32>()),
            ) else {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            };
            if hl != lineage {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            }
            match self
                .held_tenant
                .iter()
                .find(|r| r.zone == zone && r.lineage == lineage && r.gen == gen && r.seq == seq)
            {
                None => {
                    return ok(Err(Verdict::Pending(
                        "ref-unresolved",
                        "pending-dependency",
                    )))
                }
                Some(r) if r.op_hash != hop => {
                    return Err(Unimplemented("carried head mismatches the held op".into()))
                }
                Some(_) => caps.push((gen, seq)),
            }
        }
        ok(Ok(caps))
    }

    /// Is `(gen, seq)` at or below one of the carried caps?
    fn at_or_below(caps: &[(u64, u64)], gen: u64, seq: u64) -> bool {
        caps.iter().any(|&(g, s)| g == gen && seq <= s)
    }

    /// Does the held op stand against every boundary — the revoke
    /// selectors and each recovery's named-or-blanket rule?
    fn op_standing(&self, rec: &HeldTenantOp) -> bool {
        for b in &self.tenant_boundaries {
            if b.zone == rec.zone
                && b.lineage == rec.lineage
                && b.selector_grant.is_none_or(|g| g == rec.cited_grant)
                && !Self::at_or_below(&b.caps, rec.gen, rec.seq)
            {
                return false;
            }
        }
        for r in &self.recoveries {
            if !r.lineages_at_base.contains(&rec.lineage) {
                // Enrolled after the recovery: new authority under
                // the surviving chain — folds normally.
                continue;
            }
            match r
                .named
                .iter()
                .find(|n| n.zone == rec.zone && n.lineage == rec.lineage)
            {
                Some(n) => {
                    if !Self::at_or_below(&n.caps, rec.gen, rec.seq) {
                        return false;
                    }
                }
                // The omission blanket: the implicit revivable
                // `"none"` override quarantines the pair's entire
                // tenant history (D-132/D-151).
                None => return false,
            }
        }
        true
    }

    /// Is the key's effective owner FROZEN (D-155)? Implemented arm:
    /// a matching authority-ending frontier closed the owner's
    /// authority's remaining claim room — the owner's citing grant is
    /// revoked with the owner preserved at-or-below its cutoff. (The
    /// effect-finality arm has no engine state yet; the D-161/D-169
    /// reservation clause aborts honestly when an order-earlier
    /// claimant exists — no vector pins that composition.)
    fn owner_frozen(
        &self,
        owner: &HeldTenantOp,
        claimants: &[&HeldTenantOp],
    ) -> Result<bool, Unimplemented> {
        if claimants
            .iter()
            .take_while(|c| c.op_hash != owner.op_hash)
            .count()
            > 0
        {
            return Err(Unimplemented(
                "freeze reservation with order-earlier claimants".into(),
            ));
        }
        let Some(g) = self.grants.iter().find(|g| g.h_grant == owner.cited_grant) else {
            return Ok(false);
        };
        Ok(g.revoked
            && g.revoke_caps
                .as_ref()
                .is_some_and(|caps| Self::at_or_below(caps, owner.gen, owner.seq)))
    }

    /// The §10.5 derived lanes: re-classify every held tenant
    /// operation against the current boundaries and claimant folds.
    /// Returns op_hash → verdict.
    pub(crate) fn derived_tenant_verdicts(
        &self,
    ) -> Result<BTreeMap<[u8; 32], Verdict>, Unimplemented> {
        let mut out = BTreeMap::new();
        for rec in &self.held_tenant {
            let v = if !self.grants.iter().any(|g| g.h_grant == rec.cited_grant) {
                // The citing grant dissolved with a cut branch: the
                // held bytes await their dependency again (the D-199
                // lane; D-138 — everything a cut fact derived
                // re-evaluates).
                Verdict::Pending("ref-unresolved", "pending-dependency")
            } else if !self.op_standing(rec) {
                Verdict::Rejected("cutoff", "quarantine-reproposal")
            } else if let Some(imp) = &rec.import {
                // The claimant fold: total portable order
                // (grant control position, gen, seq); the effective
                // owner is the order's first STANDING claimant.
                let mut claimants: Vec<&HeldTenantOp> = self
                    .held_tenant
                    .iter()
                    .filter(|c| c.import.as_ref().is_some_and(|i| i.key == imp.key))
                    .collect();
                claimants.sort_by_key(|c| {
                    (c.import.as_ref().expect("filtered").grant_pos, c.gen, c.seq)
                });
                let owner = *claimants
                    .iter()
                    .find(|c| self.op_standing(c))
                    .expect("rec itself stands");
                if owner.op_hash == rec.op_hash {
                    Verdict::Admitted
                } else if self.owner_frozen(owner, &claimants)? {
                    // A claim against a frozen owner can never win
                    // while the basis stands (D-161/D-169/D-196).
                    Verdict::Rejected("import-collision", "quarantine-reproposal")
                } else {
                    // An order-loser against an UNFROZEN owner is
                    // ordinary displacement — outcome unpinned.
                    return Err(Unimplemented("unfrozen order-loser".into()));
                }
            } else {
                Verdict::Admitted
            };
            out.insert(rec.op_hash, v);
        }
        Ok(out)
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

    /// `c.zone_policy` — install a full ZonePolicy (§7.1, D-28:
    /// policy never changes implicitly); acceptance advances the
    /// zone's capability epoch by 1 (D-69) under the same
    /// union-coverage rule as a bare bump; the new policy governs
    /// operations signed at the new epoch onward (§9.4 anchoring).
    fn admit_zone_policy(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        if let Err(v) = self.ctrl_admin_preamble(op) {
            return ok(Err(v));
        }
        let body = &op.body;
        let bad = || Verdict::Rejected("body-invariant", "reject-permanent");
        let Some(policy) = body.get("policy") else {
            return ok(Err(bad()));
        };
        if policy.get("v").and_then(|n| n.as_uint()) != Some(1) {
            return ok(Err(bad()));
        }
        let Some(zone) = b16_field(policy, "zone_id") else {
            return ok(Err(bad()));
        };
        let strictness = policy.get("strictness").and_then(|n| n.as_text());
        if !matches!(strictness, Some("strict") | Some("lenient")) {
            return ok(Err(bad()));
        }
        let fallback = policy.get("deadline_fallback").and_then(|n| n.as_text());
        let require_cert = policy
            .get("require_cert_deadlines")
            .and_then(|n| n.as_bool());
        match (fallback, require_cert) {
            // D-76 cross-field: fail-closed REQUIRES the certificate
            // half.
            (Some("fail-closed"), Some(true)) | (Some("budgets"), Some(_)) => {}
            _ => return ok(Err(bad())),
        }
        let witnesses = policy_witness_devices(Some(policy))?;
        if witnesses.len() > 64 {
            return ok(Err(bad()));
        }
        // connect_service_key is REQUIRED iff "connect" is listed
        // (D-70) — "connect" itself has no engine yet, so a present
        // key can never be legal here.
        if policy.get("connect_service_key").is_some() {
            return ok(Err(bad()));
        }
        let Some(&cur) = self.cap_epochs.get(&zone) else {
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        if self.zone_strict.get(&zone) != Some(&true) {
            return Err(Unimplemented("non-strict zone coverage".into()));
        }
        // Closure entries + union coverage — the bump's rule (D-153).
        let live = self.live_lineages(zone);
        let mut entries: Vec<[u8; 16]> = Vec::new();
        if let Some(cs) = body.get("cutoffs") {
            let Some(cs) = cs.as_array() else {
                return ok(Err(bad()));
            };
            for cn in cs {
                let (Some(cz), Some(cl)) = (b16_field(cn, "zone_id"), b16_field(cn, "lineage"))
                else {
                    return ok(Err(bad()));
                };
                if cz != zone || !live.contains(&cl) {
                    return ok(Err(bad()));
                }
                match cn.get("heads").and_then(|h| h.as_array()) {
                    None => return ok(Err(bad())),
                    Some(a) if !a.is_empty() => {
                        return Err(Unimplemented("frontierclose heads".into()))
                    }
                    Some(_) => entries.push(cl),
                }
            }
        }
        let covered = |l: &[u8; 16]| {
            entries.contains(l)
                || self
                    .staged_closes
                    .iter()
                    .any(|&(sz, sl)| sz == zone && sl == *l)
        };
        if live.iter().any(|l| !covered(l)) {
            return ok(Err(bad()));
        }

        // Accept: advance the epoch, consume the zone's stages
        // one-shot, and anchor the NEW policy at the new epoch.
        self.cap_epochs.insert(zone, cur + 1);
        self.staged_closes.retain(|&(sz, _)| sz != zone);
        self.zone_strict.insert(zone, strictness == Some("strict"));
        self.policies.insert((zone, cur + 1), witnesses);
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

    /// `c.zone_create` — a fresh zone opening at KEK epoch 1 and
    /// capability epoch 1 with its wrap set and policy.
    fn admit_zone_create(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        if let Err(v) = self.ctrl_admin_preamble(op) {
            return ok(Err(v));
        }
        let body = &op.body;
        let bad = || Verdict::Rejected("body-invariant", "reject-permanent");
        let Some(zone_id) = b16_field(body, "zone_id") else {
            return ok(Err(bad()));
        };
        if self.zones.contains(&zone_id)
            || body.get("initial_epoch").and_then(|n| n.as_uint()) != Some(1)
        {
            return ok(Err(bad()));
        }
        let Some(policy) = body.get("zone_policy") else {
            return ok(Err(bad()));
        };
        if b16_field(policy, "zone_id") != Some(zone_id) {
            return ok(Err(bad()));
        }
        let plane = self
            .plane_id
            .expect("admin key resolved ⇒ genesis installed");
        let wraps = match body.get("wraps").and_then(|w| w.as_array()) {
            Some(a) if !a.is_empty() => a,
            _ => return ok(Err(bad())),
        };
        let mut recipients = Vec::new();
        for wn in wraps {
            match self.check_wrap(wn, plane, zone_id, 1, None)? {
                Ok(r) => recipients.push(r),
                Err(v) => return ok(Err(v)),
            }
        }

        // Accept.
        self.zones.push(zone_id);
        self.kek_epochs.insert(zone_id, 1);
        self.cap_epochs.insert(zone_id, 1);
        let strict = policy.get("strictness").and_then(|s| s.as_text()) == Some("strict");
        self.zone_strict.insert(zone_id, strict);
        self.policies
            .insert((zone_id, 1), policy_witness_devices(Some(policy))?);
        for r in recipients {
            self.record_wrap(zone_id, 1, r);
        }
        self.ctrl_next_seq += 1;
        self.ctrl_head = op.op_hash();
        ok(Ok(()))
    }

    /// `c.space_create` — the body IS a `spacedef`. The status-policy
    /// reference is recorded structurally; pinning its hash against
    /// the B.2/B.3 literals is the surfaces phase's job.
    fn admit_space_create(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        if let Err(v) = self.ctrl_admin_preamble(op) {
            return ok(Err(v));
        }
        let body = &op.body;
        let bad = || Verdict::Rejected("body-invariant", "reject-permanent");
        let (Some(space_id), Some(zone_id)) =
            (b16_field(body, "space_id"), b16_field(body, "zone_id"))
        else {
            return ok(Err(bad()));
        };
        if !self.zones.contains(&zone_id) {
            // The zone's creation may arrive later (register #24).
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        }
        if self.spaces.iter().any(|s| s.space_id == space_id) {
            return ok(Err(bad()));
        }
        if let Some(v) = self.record_space(body, zone_id)? {
            return ok(Err(v));
        }

        // Accept.
        self.ctrl_next_seq += 1;
        self.ctrl_head = op.op_hash();
        ok(Ok(()))
    }

    /// `c.recovery_succession` (§7.4 C3′), the base-at-head shape:
    /// recovery-arm signature against the current commitment,
    /// `repoch = current + 1`, `epoch >` the epoch at base; named
    /// `tenant_cutoffs` preserve at-or-below, every omitted
    /// base-enrolled `(zone, lineage)` folds the revivable blanket.
    /// Branch cutting (base below the head), storage adoption, and
    /// the D-150 freshness carriage abort honestly.
    /// `c.drill` — a recovery-signed nonce statement (§7.1): the
    /// recovery-arm signature against the CURRENT commitment at the
    /// CURRENT repoch (a proof, not a succession; "trusted lane" is
    /// product guidance, not a portable predicate).
    fn admit_drill(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        if let Err(v) = Self::ctrl_header_pins(op) {
            return ok(Err(v));
        }
        if let Err(v) = self.ctrl_chain(op) {
            return ok(Err(v));
        }
        let Proof::Recovery {
            repoch,
            recovery_pk,
        } = op.header.proof
        else {
            return ok(Err(Verdict::Rejected("proof-arm", "reject-permanent")));
        };
        let Ok(recovery_pk) = <[u8; 32]>::try_from(recovery_pk) else {
            return ok(Err(Verdict::Rejected("proof-arm", "reject-permanent")));
        };
        let Some(commitment) = self.recovery_commitment else {
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        if repoch != self.repoch || domains::h("drill", &recovery_pk) != commitment {
            return ok(Err(Verdict::Rejected("proof-arm", "reject-permanent")));
        }
        if !op.verify_ed25519(&recovery_pk)
            || op.header.signer_key_id != domains::key_id("ed25519", &recovery_pk)
        {
            return ok(Err(Verdict::Rejected("sig-invalid", "reject-permanent")));
        }
        if !op.body_hash_ok() {
            return ok(Err(Verdict::Rejected("body-hash", "reject-permanent")));
        }
        if op
            .body
            .get("nonce")
            .and_then(|n| n.bytes_n::<16>())
            .is_none()
        {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }
        self.ctrl_next_seq += 1;
        self.ctrl_head = op.op_hash();
        ok(Ok(()))
    }

    fn admit_recovery(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        if let Err(v) = Self::ctrl_header_pins(op) {
            return ok(Err(v));
        }
        // NO ctrl_chain gate: placement is frozen to base.seq + 1
        // and a valid recovery never triggers C2 against cut
        // branches (the §7.4 precedence exception) — the base checks
        // below are the whole position rule.
        let bad = || Verdict::Rejected("body-invariant", "reject-permanent");
        let Proof::Recovery {
            repoch,
            recovery_pk,
        } = op.header.proof
        else {
            return ok(Err(Verdict::Rejected("proof-arm", "reject-permanent")));
        };
        let Ok(recovery_pk) = <[u8; 32]>::try_from(recovery_pk) else {
            return ok(Err(Verdict::Rejected("proof-arm", "reject-permanent")));
        };
        // The revealed key must match the CURRENT commitment.
        let Some(commitment) = self.recovery_commitment else {
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        if domains::h("drill", &recovery_pk) != commitment {
            return ok(Err(Verdict::Rejected("proof-arm", "reject-permanent")));
        }
        if !op.verify_ed25519(&recovery_pk)
            || op.header.signer_key_id != domains::key_id("ed25519", &recovery_pk)
        {
            return ok(Err(Verdict::Rejected("sig-invalid", "reject-permanent")));
        }
        if !op.body_hash_ok() {
            return ok(Err(Verdict::Rejected("body-hash", "reject-permanent")));
        }
        let body = &op.body;
        let Some(base) = body.get("base") else {
            return ok(Err(bad()));
        };
        let (Some(base_seq), Some(base_op)) = (
            base.get("seq").and_then(|n| n.as_uint()),
            base.get("op").and_then(|n| n.bytes_n::<32>()),
        ) else {
            return ok(Err(bad()));
        };
        // Placement is frozen: seq = base.seq + 1, prev = base.op.
        let h = &op.header;
        if h.writer_sequence != base_seq + 1 || h.previous_writer_hash != base_op {
            return ok(Err(bad()));
        }
        // Base resolution: at the head = pure succession (nothing
        // cuts); below it = the branch cut (D-138 total re-fold);
        // beyond it or hash-mismatched = the base is not held.
        let head_seq = self.ctrl_next_seq.max(1) - 1;
        let cut_below_head = if base_seq == head_seq && base_op == self.ctrl_head {
            false
        } else if base_seq >= 1 && base_seq < head_seq {
            let held = self
                .ctrl_log
                .iter()
                .find(|(s, _)| *s == base_seq)
                .and_then(|(_, b)| parse_op(b).ok())
                .map(|o| o.op_hash());
            if held != Some(base_op) {
                return ok(Err(bad()));
            }
            true
        } else {
            // The named base position is not held yet (or its hash
            // mismatches a shorter chain): await it.
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        // repoch = current + 1 (proof arm and body agree).
        if body.get("repoch").and_then(|n| n.as_uint()) != Some(repoch) || repoch != self.repoch + 1
        {
            return ok(Err(bad()));
        }
        // epoch > the epoch at base.
        let Some(epoch) = body.get("epoch").and_then(|n| n.as_uint()) else {
            return ok(Err(bad()));
        };
        let current_admin = self.admin_keys.keys().max().copied().unwrap_or(0);
        if epoch <= current_admin {
            return ok(Err(bad()));
        }
        let Some(new_admin) = body.get("new_admin") else {
            return ok(Err(bad()));
        };
        if new_admin.get("alg").and_then(|a| a.as_text()) != Some("ed25519") {
            return Err(Unimplemented("non-ed25519 successor admin key".into()));
        }
        let Some(new_admin_pk) = new_admin.get("pk").and_then(|n| n.bytes_n::<32>()) else {
            return ok(Err(bad()));
        };
        let Some(new_commitment) = body
            .get("new_recovery_commitment")
            .and_then(|n| n.bytes_n::<32>())
        else {
            return ok(Err(bad()));
        };
        if body.get("adopted_renewals").is_some() || body.get("retired_keys").is_some() {
            return Err(Unimplemented("recovery renewal/freshness carriage".into()));
        }
        match body.get("adopted_rotations").and_then(|a| a.as_array()) {
            Some([]) => {}
            Some(_) => return Err(Unimplemented("adopted rotations".into())),
            None => return ok(Err(bad())),
        }
        // Named tenant cutoffs: recover-purpose frontiercloses whose
        // carried heads resolve against the held chains.
        let Some(cutoffs) = body.get("tenant_cutoffs").and_then(|c| c.as_array()) else {
            return ok(Err(bad()));
        };
        let mut named = Vec::new();
        for cn in cutoffs {
            let (Some(cz), Some(cl)) = (b16_field(cn, "zone_id"), b16_field(cn, "lineage")) else {
                return ok(Err(bad()));
            };
            match self.parse_heads(cn, cz, cl)? {
                Ok(caps) => named.push(TenantBoundary {
                    zone: cz,
                    lineage: cl,
                    selector_grant: None,
                    caps,
                }),
                Err(v) => return ok(Err(v)),
            }
        }

        // Accept. A below-head base first cuts the branch above it:
        // the control re-fold rebuilds every control-derived fact
        // from the surviving prefix, and the cut ops classify
        // (cutoff, quarantine-reproposal) — the D-140 recover
        // boundary reading (a register/audit decision; the spec
        // names no pair for a cut CONTROL op). Tenant re-derivation
        // is the derived lanes' job either way.
        if cut_below_head {
            self.refold_control(
                base_seq,
                Verdict::Rejected("cutoff", "quarantine-reproposal"),
            )?;
        }
        self.ctrl_frozen = false; // only recovery resolves C2
        self.recovery_placement = Some(op.header.writer_sequence);
        self.admin_keys.insert(epoch, new_admin_pk);
        self.repoch = repoch;
        self.recovery_commitment = Some(new_commitment);
        self.recoveries.push(RecoveryState {
            named,
            lineages_at_base: self.lineages.iter().map(|(l, _)| *l).collect(),
        });
        self.ctrl_next_seq += 1;
        self.ctrl_head = op.op_hash();
        ok(Ok(()))
    }

    /// The shared tenant preamble under the dev arm (D-199: unheld
    /// citations pend): citation resolution, signature, O8 actor,
    /// grant scope on every axis, §9.3 chain arithmetic, epochs.
    /// Mutates nothing; the resolved grant comes back for the
    /// per-verb body stage.
    fn tenant_preamble(
        &self,
        op: &SignedOp,
        verb: &str,
    ) -> Result<Result<HeldGrant, Verdict>, Unimplemented> {
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
        // Post-revocation citations need D-86 position-relative
        // validity (the signed-before-the-boundary prefix stands) — a
        // later slice; no tranche fixture cites across a revocation.
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

        // Proof stage: grant scope (tenant ∧ zone ∧ space ∧ op),
        // lineage binding.
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
        if !grant.verbs.iter().any(|v| v == verb) {
            return ok(Err(Verdict::Rejected("scope-op", "reject-permanent")));
        }
        if grant.lineage != Some(h.writer_lineage) {
            return ok(Err(Verdict::Rejected("no-grant", "reject-permanent")));
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

        // Epochs (D-78 portable currency): a signed epoch the chain
        // has not opened pends `epoch-unopened`; the reserved value 0
        // is read-only-wildcard territory — a write op using it is
        // `body-invariant`; grant slack is a signed-vs-signed lower
        // bound (`capability-epoch`, revivable).
        if h.capability_epoch == 0 || h.authored_kek_epoch == 0 {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }
        let (Some(&zone_cap), Some(&zone_kek)) = (
            self.cap_epochs.get(&h.zone_id),
            self.kek_epochs.get(&h.zone_id),
        ) else {
            // The zone's creation may arrive later (register #24).
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        if h.capability_epoch > zone_cap || h.authored_kek_epoch > zone_kek {
            return ok(Err(Verdict::Pending(
                "epoch-unopened",
                "pending-dependency",
            )));
        }
        if grant.capability_epoch > h.capability_epoch {
            return ok(Err(Verdict::Rejected(
                "capability-epoch",
                "quarantine-reproposal",
            )));
        }

        // Time stage (§9.1 deadlines / §4.7 T5 leases) — only where
        // deadline fields exist (D-28); a present deadline always
        // binds, in every posture (D-12/D-56).
        let mut deadlines: Vec<u64> = Vec::new();
        deadlines.extend(grant.expiry_deadline_ms);
        deadlines.extend(held_cert.expiry_deadline_ms);
        if !deadlines.is_empty() || grant.online_lease {
            let accepts = self.qualified_accepts(op, &held_cert);
            for d in deadlines {
                if !accepts.iter().any(|&seen| seen <= d) {
                    return ok(Err(Verdict::Pending(
                        "deadline-unreceipted",
                        "pending-dependency",
                    )));
                }
            }
            if grant.online_lease {
                let max_age = grant.max_age_ms.unwrap_or(0);
                let windows = self.qualified_lease_windows(op, &held_cert, &grant, max_age);
                if windows.is_empty() {
                    return ok(Err(Verdict::Pending("lease-missing", "pending-dependency")));
                }
                // T5: skew = 300000 ms, fixed.
                const SKEW_MS: u64 = 300_000;
                let in_window = windows
                    .iter()
                    .any(|&(i, e)| accepts.iter().any(|&s| s >= i && s <= e + SKEW_MS));
                if !in_window {
                    if accepts.is_empty() {
                        // No observation held at all: still awaiting
                        // evidence (pending, §9.1's pairing).
                        return ok(Err(Verdict::Pending("lease-missing", "pending-dependency")));
                    }
                    // A held qualified observation OUTSIDE every
                    // valid window is conclusive staleness.
                    return ok(Err(Verdict::Rejected(
                        "lease-stale",
                        "quarantine-reproposal",
                    )));
                }
            }
        }
        ok(Ok(grant))
    }

    /// The T2 anchor: the policy in force at `(zone, epoch)` —
    /// absent epochs resolve downward (a bare bump carries
    /// policy(e−1) forward, §9.4).
    fn witnesses_at(&self, zone: [u8; 16], epoch: u64) -> Vec<[u8; 16]> {
        self.policies
            .range((zone, 0)..=(zone, epoch))
            .next_back()
            .map(|(_, w)| w.clone())
            .unwrap_or_default()
    }

    /// §9.1/T2-qualified `accept` receipts for THIS operation: plane
    /// and zone bound, subject resolved through the §5.6 index,
    /// issuer a held cert of a DIFFERENT device that the operation's
    /// anchored policy lists, signature valid. Returns `seen_ms`
    /// values.
    fn qualified_accepts(&self, op: &SignedOp, signer: &HeldCert) -> Vec<u64> {
        let h = &op.header;
        let witnesses = self.witnesses_at(h.zone_id, h.capability_epoch);
        let op_hash = op.op_hash();
        self.receipts
            .iter()
            .filter_map(|r| {
                if Some(r.plane) != self.plane_id || r.zone != h.zone_id {
                    return None;
                }
                if self.item_index.get(&r.subject) != Some(&op_hash) {
                    return None;
                }
                let cert = self.certs.iter().find(|c| c.h_cert == r.issuer_cert)?;
                if cert.device_id == signer.device_id {
                    return None; // T2: a signer never receipts itself
                }
                if !witnesses.contains(&cert.device_id) {
                    return None;
                }
                verify_stmt(&cert.sig_pk, "receipt", &r.stmt_raw, &r.sig).then_some(r.seen_ms)
            })
            .collect()
    }

    /// T5-valid, T2-qualified lease windows for `(grant_id,
    /// lineage)`: `expires − issued ≤ max_age_ms`, same
    /// qualification predicate as receipts.
    fn qualified_lease_windows(
        &self,
        op: &SignedOp,
        signer: &HeldCert,
        grant: &HeldGrant,
        max_age: u64,
    ) -> Vec<(u64, u64)> {
        let h = &op.header;
        let witnesses = self.witnesses_at(h.zone_id, h.capability_epoch);
        self.leases
            .iter()
            .filter_map(|l| {
                if Some(l.plane) != self.plane_id || l.zone != h.zone_id {
                    return None;
                }
                if l.grant_id != grant.grant_id || l.lineage != h.writer_lineage {
                    return None;
                }
                if l.expires_ms < l.issued_ms || l.expires_ms - l.issued_ms > max_age {
                    return None;
                }
                let cert = self.certs.iter().find(|c| c.h_cert == l.issuer_cert)?;
                if cert.device_id == signer.device_id {
                    return None;
                }
                if !witnesses.contains(&cert.device_id) {
                    return None;
                }
                verify_stmt(&cert.sig_pk, "lease", &l.stmt_raw, &l.sig)
                    .then_some((l.issued_ms, l.expires_ms))
            })
            .collect()
    }

    /// The D-138 total re-fold: rebuild every control-derived field
    /// by replaying the surviving control prefix (`keep` entries)
    /// into a fresh fold; tenant-side and aux state carry over; the
    /// cut suffix joins `cut_chain` and the overlay under
    /// `cut_verdict`. Incremental implementations MUST converge to
    /// the fresh-fold result — this IS the fresh fold.
    fn refold_control(
        &mut self,
        keep_through: u64,
        cut_verdict: Verdict,
    ) -> Result<(), Unimplemented> {
        let mut fresh = State {
            receipts: std::mem::take(&mut self.receipts),
            leases: std::mem::take(&mut self.leases),
            item_index: std::mem::take(&mut self.item_index),
            tenant_chains: std::mem::take(&mut self.tenant_chains),
            held_tenant: std::mem::take(&mut self.held_tenant),
            erase_queue: std::mem::take(&mut self.erase_queue),
            retrieval_excluded: std::mem::take(&mut self.retrieval_excluded),
            ctrl_overlay: std::mem::take(&mut self.ctrl_overlay),
            cut_chain: std::mem::take(&mut self.cut_chain),
            // Tenant replay keys survive; control ones re-derive on
            // replay (replay-key ownership is derived state, D-155).
            request_seen: std::mem::take(&mut self.request_seen)
                .into_iter()
                .filter(|((_, lineage, _), _)| *lineage != CTRL_LINEAGE)
                .collect(),
            ..State::default()
        };
        let mut log = std::mem::take(&mut self.ctrl_log);
        log.sort_by_key(|(seq, _)| *seq);
        for (seq, bytes) in &log {
            if *seq > keep_through {
                if let Ok(op) = parse_op(bytes) {
                    let hash = op.op_hash();
                    fresh.cut_chain.entry(*seq).or_default().push(hash);
                    fresh.ctrl_overlay.insert(hash, cut_verdict);
                }
                continue;
            }
            let op = parse_op(bytes)
                .map_err(|_| Unimplemented("re-fold parse of an accepted op".into()))?;
            match fresh.admit(&op)? {
                Ok(()) => {
                    fresh.request_seen.insert(
                        (
                            op.header.zone_id,
                            op.header.writer_lineage,
                            op.header.request_id,
                        ),
                        op.op_hash(),
                    );
                    fresh.ctrl_log.push((*seq, bytes.clone()));
                }
                Err(_) => {
                    return Err(Unimplemented(
                        "re-fold divergence: a surviving op no longer admits".into(),
                    ))
                }
            }
        }
        *self = fresh;
        Ok(())
    }

    /// C2 at position `seq`: BOTH branches freeze — the chain
    /// re-folds to the pre-fork prefix, every op at or beyond the
    /// contested position (either branch) classifies `(ctrl-fork,
    /// freeze-control)`, and no further control op admits until a
    /// recovery resolves.
    fn freeze_at(&mut self, seq: u64) -> Result<(), Unimplemented> {
        self.refold_control(seq - 1, Verdict::Rejected("ctrl-fork", "freeze-control"))?;
        self.ctrl_frozen = true;
        Ok(())
    }

    /// The pre-admission control gate (C2/C5, §7.4): a frozen plane
    /// admits no control op but the resolving recovery; a differing
    /// op at a HELD position is cut-branch material where a cut or
    /// recovery covers it, and fork evidence otherwise.
    fn ctrl_fork_gate(&mut self, op: &SignedOp) -> Result<Option<Verdict>, Unimplemented> {
        if self.ctrl_frozen {
            return Ok(Some(Verdict::Rejected("ctrl-fork", "freeze-control")));
        }
        let seq = op.header.writer_sequence;
        if seq >= self.ctrl_next_seq.max(1) {
            return Ok(None);
        }
        if self.pending_compounds.contains_key(&op.op_hash()) {
            // The op holds its OWN reserved position (a pending
            // c.revoke_device compound, D-195) — re-evaluation, not
            // a fork.
            return Ok(None);
        }
        // Byte-identical replay was consumed by the replay registry;
        // this op DIFFERS at a held position.
        if self
            .cut_chain
            .get(&seq)
            .is_some_and(|c| c.contains(&op.op_hash()))
            || self.recovery_placement == Some(seq)
        {
            // A cut-branch op, or a challenger at the recovery's own
            // position (the precedence exception: recovery wins).
            return Ok(Some(Verdict::Rejected("cutoff", "quarantine-reproposal")));
        }
        self.freeze_at(seq)?;
        Ok(Some(Verdict::Rejected("ctrl-fork", "freeze-control")))
    }

    /// The walkthrough probe registry (register #17: fixture-named
    /// canonical CBOR of the derived construct).
    pub fn probe(&self, name: &str) -> Option<Vec<u8>> {
        match name {
            "plane.provenance" => {
                let p = self.provenance.as_deref()?;
                let mut out = vec![0x60 | p.len() as u8];
                out.extend_from_slice(p.as_bytes());
                Some(out)
            }
            "ceiling.lifted" => Some(vec![if self.repoch > 0 { 0xf5 } else { 0xf4 }]),
            "ctrl.head" => {
                let mut out = vec![0x58, 32];
                out.extend_from_slice(&self.ctrl_head);
                Some(out)
            }
            "ctrl.frozen" => Some(vec![if self.ctrl_frozen { 0xf5 } else { 0xf4 }]),
            "repoch" => Some(crate::kat::encode(&crate::kat::Enc::U(self.repoch))),
            _ => None,
        }
    }

    /// Install the vector's held context (the fold lane's `aux`):
    /// the §5.6 `index` plus `Signed<…>` receipts and leases (§4.7).
    /// Aux is STATE, not events — nothing here folds.
    pub(crate) fn install_aux(
        &mut self,
        aux: &BTreeMap<String, Vec<u8>>,
    ) -> Result<(), Unimplemented> {
        for (name, bytes) in aux {
            let Ok(node) = crate::cbor::decode(bytes) else {
                return Err(Unimplemented(format!("aux {name}: not canonical CBOR")));
            };
            if name == "index" {
                let Some(entries) = node.as_array() else {
                    return Err(Unimplemented("aux index: not an array".into()));
                };
                for e in entries {
                    let (Some(addr), Some(op)) = (
                        e.get("item_addr").and_then(|n| n.bytes_n::<32>()),
                        e.get("op").and_then(|n| n.bytes_n::<32>()),
                    ) else {
                        return Err(Unimplemented("aux index entry shape".into()));
                    };
                    self.item_index.insert(addr, op);
                }
                continue;
            }
            let (Some(stmt), Some(sig)) = (node.get("stmt"), node.get("sig")) else {
                return Err(Unimplemented(format!("aux {name}: unrecognized shape")));
            };
            let Some(sig) = sig.as_bytes() else {
                return Err(Unimplemented(format!("aux {name}: sig shape")));
            };
            let Some(issuer) = stmt.get("issuer") else {
                return Err(Unimplemented(format!("aux {name}: no issuer")));
            };
            if issuer.get("src").and_then(|n| n.as_text()) != Some("device") {
                return Err(Unimplemented("service-issued statements".into()));
            }
            let (Some(issuer_cert), Some(plane), Some(zone)) = (
                issuer.get("cert").and_then(|n| n.bytes_n::<32>()),
                stmt.get("plane_id").and_then(|n| n.bytes_n::<32>()),
                stmt.get("zone_id").and_then(|n| n.bytes_n::<16>()),
            ) else {
                return Err(Unimplemented(format!("aux {name}: statement binding")));
            };
            if let Some(kind) = stmt.get("kind").and_then(|k| k.as_text()) {
                if kind == "accept" {
                    let (Some(subject), Some(seen_ms)) = (
                        stmt.get("subject").and_then(|n| n.bytes_n::<32>()),
                        stmt.get("seen_ms").and_then(|n| n.as_uint()),
                    ) else {
                        return Err(Unimplemented(format!("aux {name}: accept shape")));
                    };
                    self.receipts.push(AuxReceipt {
                        issuer_cert,
                        plane,
                        zone,
                        subject,
                        seen_ms,
                        stmt_raw: stmt.raw.to_vec(),
                        sig: sig.to_vec(),
                    });
                }
                // storage/replica/witness receipts have no fold
                // consumer yet — held, inert.
            } else if stmt.get("grant_id").is_some() {
                let (Some(grant_id), Some(lineage), Some(issued_ms), Some(expires_ms)) = (
                    stmt.get("grant_id").and_then(|n| n.bytes_n::<16>()),
                    stmt.get("lineage").and_then(|n| n.bytes_n::<16>()),
                    stmt.get("issued_ms").and_then(|n| n.as_uint()),
                    stmt.get("expires_ms").and_then(|n| n.as_uint()),
                ) else {
                    return Err(Unimplemented(format!("aux {name}: lease shape")));
                };
                self.leases.push(AuxLease {
                    issuer_cert,
                    plane,
                    zone,
                    grant_id,
                    lineage,
                    issued_ms,
                    expires_ms,
                    stmt_raw: stmt.raw.to_vec(),
                    sig: sig.to_vec(),
                });
            } else {
                return Err(Unimplemented(format!("aux {name}: unrecognized statement")));
            }
        }
        Ok(())
    }

    /// §7.5: the hosted ceiling binds until ANY valid recovery
    /// succession is accepted (the lift is portable by definition,
    /// D-42 — repoch advances exactly there).
    fn hosted_ceiling_active(&self) -> bool {
        self.provenance.as_deref() == Some("hosted") && self.repoch == 0
    }

    /// §11.4 derived actor class. Human evidence + full judgment
    /// rights = owner; human otherwise = safe-human; an attested
    /// actor = session (shape 2); service kind = service. A BARE
    /// non-human unattested writer (autonomous daemon/browser) has
    /// no §11.4 row — derived as `session` here (the closest §10.1
    /// reading; a register/audit item, not settled prose).
    fn actor_class(op: &SignedOp, grant: &HeldGrant) -> &'static str {
        let h = &op.header;
        let human = h.actor_kind == "human" && h.attested_by.is_none();
        if human {
            let full = grant
                .verbs
                .iter()
                .any(|v| matches!(v.as_str(), "judge.full" | "pin.full" | "curate.instruction"));
            if full {
                "owner"
            } else {
                "safe-human"
            }
        } else if h.actor_kind == "service" {
            "service"
        } else {
            "session"
        }
    }

    /// Accept a tenant op into its chain and the held registry. The
    /// held record's FOLD verdict is thereafter derived (§10.5).
    fn record_tenant(
        &mut self,
        op: &SignedOp,
        grant: &HeldGrant,
        claim: Option<(String, String, u8)>,
        release: Option<ReleaseFacts>,
        import: Option<ImportFacts>,
        judge: Option<JudgeFacts>,
    ) {
        let h = &op.header;
        let key = (h.zone_id, h.writer_lineage, h.writer_gen);
        self.tenant_chains
            .insert(key, (h.writer_sequence + 1, op.op_hash()));
        self.held_tenant.push(HeldTenantOp {
            op_hash: op.op_hash(),
            zone: h.zone_id,
            space: h.space_id,
            lineage: h.writer_lineage,
            gen: h.writer_gen,
            seq: h.writer_sequence,
            cited_grant: grant.h_grant,
            actor_kind: h.actor_kind.to_string(),
            actor_id: h.actor_id.to_string(),
            human_evidence: h.actor_kind == "human" && h.attested_by.is_none(),
            actor_class: Self::actor_class(op, grant),
            claim,
            release,
            import,
            judge,
        });
    }

    /// Register a spacedef: class + the bound status policy. The
    /// polref's hash must equal the reducer's OWN derivation of the
    /// named built-in (B.2/B.3); an unknown policy id has no local
    /// table to validate against.
    fn record_space(
        &mut self,
        sd: &Node,
        zone_id: [u8; 16],
    ) -> Result<Option<Verdict>, Unimplemented> {
        let bad = Some(Verdict::Rejected("body-invariant", "reject-permanent"));
        let (Some(space_id), Some(class)) = (
            b16_field(sd, "space_id"),
            sd.get("space_class").and_then(|c| c.as_text()),
        ) else {
            return Ok(bad);
        };
        let Some(pol) = sd.get("status_policy") else {
            return Ok(bad);
        };
        let (Some(pid), Some(phash)) = (
            pol.get("id").and_then(|n| n.as_text()),
            pol.get("hash").and_then(|n| n.bytes_n::<32>()),
        ) else {
            return Ok(bad);
        };
        let Some(known) = crate::policies::policy_hash(pid) else {
            return Err(Unimplemented(format!("status policy {pid}")));
        };
        if known != phash {
            return Ok(bad);
        }
        self.spaces.push(SpaceInfo {
            space_id,
            zone_id,
            space_class: class.to_string(),
            policy_id: pid.to_string(),
            policy_hash: phash,
        });
        Ok(None)
    }

    /// Tenant `m.claim` (plain propose).
    fn admit_claim(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        let grant = match self.tenant_preamble(op, "propose")? {
            Ok(g) => g,
            Err(v) => return ok(Err(v)),
        };
        let body = &op.body;
        let kind = body.get("kind").and_then(|k| k.as_text()).unwrap_or("");
        if let Some(kinds) = &grant.kinds {
            if !kinds.iter().any(|k| k == kind) {
                return ok(Err(Verdict::Rejected("scope-kind", "reject-permanent")));
            }
        }
        let statement = body
            .get("statement")
            .and_then(|s| s.as_text())
            .unwrap_or("");
        let Some(sens) = body
            .get("sensitivity")
            .and_then(|s| s.as_text())
            .and_then(class_rank)
        else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        // sensitivity ≤ ceilings (§11.1 row).
        if grant.class_ceiling.is_some_and(|c| sens > c) {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }

        self.record_tenant(
            op,
            &grant,
            Some((kind.to_string(), statement.to_string(), sens)),
            None,
            None,
            None,
        );
        ok(Ok(()))
    }

    /// Tenant `m.export.release` (§11.8): flow matching is
    /// existential and whole; `class_floor = max effective(sources)`
    /// ≤ min(flow ceiling, grant ceiling); sources are held claims.
    /// The stamp (`data_frontier`/`control_frontier`/`as_of_ms`) is
    /// carried, attested evaluation-point material — not re-verified
    /// here. Budgets (the D-98 record surcharge) have no engine state
    /// yet.
    fn admit_release(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        let grant = match self.tenant_preamble(op, "export")? {
            Ok(g) => g,
            Err(v) => return ok(Err(v)),
        };
        let body = &op.body;
        let bad = || Verdict::Rejected("body-invariant", "reject-permanent");
        let (Some(export_id), Some(content_digest), Some(to), Some(class_floor), Some(expiry)) = (
            b16_field(body, "export_id"),
            body.get("content_digest").and_then(|n| n.bytes_n::<32>()),
            body.get("to"),
            body.get("class_floor")
                .and_then(|c| c.as_text())
                .and_then(class_rank),
            body.get("expiry_deadline_ms").and_then(|n| n.as_uint()),
        ) else {
            return ok(Err(bad()));
        };
        let Some(dest_zone) = b16_field(to, "zone_id") else {
            // Egress endpoints are a governed-profile lane with no
            // fixture yet.
            return Err(Unimplemented("egress endpoint release".into()));
        };
        let Some(dest_space) = b16_field(to, "space_id") else {
            return ok(Err(bad()));
        };
        if to.get("plane_id").and_then(|n| n.bytes_n::<32>()) != self.plane_id {
            return Err(Unimplemented("cross-plane destination".into()));
        }

        // Sources: a keyed set of HELD claims (an unheld source
        // pends — D-199 spirit, register #25).
        let Some(sources) = body.get("sources").and_then(|s| s.as_array()) else {
            return ok(Err(bad()));
        };
        let mut source_hashes: Vec<[u8; 32]> = Vec::new();
        let mut max_sens: u8 = 0;
        let mut source_kinds: Vec<String> = Vec::new();
        for sn in sources {
            let Some(sh) = sn.bytes_n::<32>() else {
                return ok(Err(bad()));
            };
            if source_hashes.contains(&sh) {
                return ok(Err(bad()));
            }
            let Some(rec) = self.held_tenant.iter().find(|r| r.op_hash == sh) else {
                return ok(Err(Verdict::Pending(
                    "ref-unresolved",
                    "pending-dependency",
                )));
            };
            let Some((kind, _, sens)) = &rec.claim else {
                // Sources are claims, never judgments or pins.
                return ok(Err(bad()));
            };
            max_sens = max_sens.max(*sens);
            source_kinds.push(kind.clone());
            source_hashes.push(sh);
        }
        if source_hashes.is_empty() {
            return ok(Err(bad()));
        }

        // class_floor = max effective(sources) ≤ min(flow ceiling,
        // grant ceiling) — the flow leg rides the match below.
        if class_floor != max_sens || grant.class_ceiling.is_some_and(|c| class_floor > c) {
            return ok(Err(bad()));
        }

        // Flow matching (D-75): existential and whole — one entry
        // admits the release on every axis simultaneously.
        let h = &op.header;
        let matched = grant.flows.iter().any(|f| {
            f.from_zone == h.zone_id
                && f.from_space.is_none_or(|s| s == h.space_id)
                && f.to_raw == to.raw
                && f.kinds
                    .as_ref()
                    .is_none_or(|ks| source_kinds.iter().all(|k| ks.contains(k)))
                && f.class_ceiling >= class_floor
                && expiry <= f.expiry_deadline_ms
        });
        if !matched {
            return ok(Err(Verdict::Rejected("no-flow", "reject-permanent")));
        }

        self.record_tenant(
            op,
            &grant,
            None,
            Some(ReleaseFacts {
                export_id,
                sources: source_hashes,
                content_digest,
                dest_zone,
                dest_space,
            }),
            None,
            None,
        );
        ok(Ok(()))
    }

    /// Tenant `m.import.claim` (§11.8): per-record validation — the
    /// self-describing leaf folded up the carried path must reach the
    /// release's signed root; content equality against the LIVE
    /// source (D-134/D-198); fully-derived shape (`sensitivity ==
    /// class_floor`). The fold verdict (ownership, collision) is
    /// DERIVED — structural acceptance holds the claimant.
    fn admit_import(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        let grant = match self.tenant_preamble(op, "import")? {
            Ok(g) => g,
            Err(v) => return ok(Err(v)),
        };
        let body = &op.body;
        let bad = || Verdict::Rejected("body-invariant", "reject-permanent");
        let (Some(source_op), Some(kind), Some(statement), Some(rec_index), Some(proof_arr)) = (
            body.get("source_op").and_then(|n| n.bytes_n::<32>()),
            body.get("kind").and_then(|k| k.as_text()),
            body.get("statement").and_then(|s| s.as_text()),
            body.get("rec_index").and_then(|n| n.as_uint()),
            body.get("proof").and_then(|p| p.as_array()),
        ) else {
            return ok(Err(bad()));
        };
        let (Some(class_floor), Some(sens)) = (
            body.get("class_floor")
                .and_then(|c| c.as_text())
                .and_then(class_rank),
            body.get("sensitivity")
                .and_then(|s| s.as_text())
                .and_then(class_rank),
        ) else {
            return ok(Err(bad()));
        };
        // D-134: fully-derived content — sensitivity == class_floor.
        if sens != class_floor {
            return ok(Err(bad()));
        }
        if grant
            .kinds
            .as_ref()
            .is_some_and(|ks| !ks.iter().any(|k| k == kind))
        {
            return ok(Err(Verdict::Rejected("scope-kind", "reject-permanent")));
        }
        if grant.class_ceiling.is_some_and(|c| sens > c) {
            return ok(Err(bad()));
        }
        let Some(prov) = body.get("provenance").and_then(|p| p.get("import")) else {
            return ok(Err(bad()));
        };
        let (Some(from_plane), Some(export_id), Some(release_op), Some(digest)) = (
            prov.get("from_plane").and_then(|n| n.bytes_n::<32>()),
            b16_field(prov, "export_id"),
            prov.get("release_op").and_then(|n| n.bytes_n::<32>()),
            prov.get("digest").and_then(|n| n.bytes_n::<32>()),
        ) else {
            return ok(Err(bad()));
        };
        if Some(from_plane) != self.plane_id {
            // Cross-plane import fails closed until D0-B (D-44).
            return Err(Unimplemented("cross-plane import".into()));
        }

        // The release: a held accepted m.export.release (unheld
        // citation pends, D-199).
        let Some(rel) = self
            .held_tenant
            .iter()
            .find(|r| r.op_hash == release_op)
            .and_then(|r| r.release.as_ref())
            .cloned()
        else {
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        if rel.export_id != export_id
            || rel.content_digest != digest
            || rel.dest_zone != op.header.zone_id
            || rel.dest_space != op.header.space_id
        {
            return ok(Err(bad()));
        }
        // rec_index = the record's rank in the release's signed,
        // sorted sources (D-156).
        let Some(rank) = rel.sources.iter().position(|s| *s == source_op) else {
            return ok(Err(bad()));
        };
        if rec_index != rank as u64 {
            return ok(Err(bad()));
        }

        // Source equality against the LIVE source (D-134/D-198): the
        // carried statement/kind equal the source claim's; the floor
        // binds by equality against its effective classification.
        let Some(src) = self
            .held_tenant
            .iter()
            .find(|r| r.op_hash == source_op)
            .and_then(|r| r.claim.as_ref())
        else {
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        if src.0 != kind || src.1 != statement || src.2 != class_floor {
            return ok(Err(bad()));
        }

        // The Merkle leg: leaf + path against the signed root, exact
        // consumption (§11.8/D-162).
        let mut proof: Vec<[u8; 32]> = Vec::new();
        for pn in proof_arr {
            let Some(sib) = pn.bytes_n::<32>() else {
                return ok(Err(bad()));
            };
            proof.push(sib);
        }
        let floor_text = body
            .get("class_floor")
            .and_then(|c| c.as_text())
            .expect("checked above");
        let leaf = domains::brec_leaf(
            &export_id, rec_index, &source_op, kind, statement, floor_text,
        );
        let folded = domains::merkle_fold(leaf, rec_index, rel.sources.len() as u64, &proof);
        if folded != Some(rel.content_digest) {
            return ok(Err(bad()));
        }

        self.record_tenant(
            op,
            &grant,
            None,
            None,
            Some(ImportFacts {
                key: (from_plane, release_op, source_op),
                grant_pos: grant.ctrl_pos,
            }),
            None,
        );
        ok(Ok(()))
    }

    /// Tenant `m.erase_request` (§11.1): direct-human evidence,
    /// `targets` = claim op hashes each in grant scope (D-66).
    /// Acceptance flags the targets retrieval-excluded IMMEDIATELY
    /// and queues them for the next rotation manifest (§5.4; the
    /// D-198 deferral is the manifest-eligibility question, not the
    /// queue's).
    fn admit_erase_request(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        let grant = match self.tenant_preamble(op, "erase.request")? {
            Ok(g) => g,
            Err(v) => return ok(Err(v)),
        };
        // §10.1 shape-1: a human actor on an enrolled device with no
        // attestation. Mediated evidence shapes are a later slice.
        if op.header.actor_kind != "human" {
            return Err(Unimplemented("mediated erase evidence".into()));
        }
        let Some(targets) = op.body.get("targets").and_then(|t| t.as_array()) else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        if targets.is_empty() {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }
        let mut resolved = Vec::new();
        for tn in targets {
            let Some(t) = tn.bytes_n::<32>() else {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            };
            // The target claim may arrive later (D-199 spirit —
            // register #25; the fresh-fold order requires the pend).
            let Some(rec) = self.held_tenant.iter().find(|r| r.op_hash == t) else {
                return ok(Err(Verdict::Pending(
                    "ref-unresolved",
                    "pending-dependency",
                )));
            };
            if rec.claim.is_none() {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            }
            // In grant scope: the claim's zone/space under the CITING
            // grant.
            if grant.zone.is_some_and(|z| z != rec.zone)
                || grant
                    .spaces
                    .as_ref()
                    .is_some_and(|s| !s.contains(&rec.space))
            {
                return ok(Err(Verdict::Rejected("scope-space", "reject-permanent")));
            }
            resolved.push(t);
        }

        // Accept.
        self.record_tenant(op, &grant, None, None, None, None);
        for t in resolved {
            if !self.erase_queue.contains(&t) {
                self.erase_queue.push(t);
            }
            if !self.retrieval_excluded.contains(&t) {
                self.retrieval_excluded.push(t);
            }
        }
        ok(Ok(()))
    }

    /// Does the fold hold this operation (control or tenant)? The
    /// journal machine's opfactref resolution consults it alongside
    /// the aux set.
    pub(crate) fn holds_op(&self, h: &[u8; 32]) -> bool {
        self.held_tenant.iter().any(|r| r.op_hash == *h) || self.ctrl_head == *h
    }

    /// A held tenant op's chain coordinate — the erase machinery maps
    /// target ops to their ItemCommits through it.
    pub(crate) fn op_coordinate(&self, h: &[u8; 32]) -> Option<([u8; 16], u64, u64)> {
        self.held_tenant
            .iter()
            .find(|r| r.op_hash == *h)
            .map(|r| (r.lineage, r.gen, r.seq))
    }

    /// A held release's source set (D-198: the deferral reads the
    /// LIVE release).
    pub(crate) fn release_sources(&self, release_op: &[u8; 32]) -> Option<&[[u8; 32]]> {
        self.held_tenant
            .iter()
            .find(|r| r.op_hash == *release_op)
            .and_then(|r| r.release.as_ref())
            .map(|f| f.sources.as_slice())
    }

    /// (erase queue, retrieval-excluded) — acceptance order.
    pub(crate) fn erase_state(&self) -> (&[[u8; 32]], &[[u8; 32]]) {
        (&self.erase_queue, &self.retrieval_excluded)
    }

    /// Every held release: (op_hash, export_id, content_digest,
    /// sources) — the export-import lane re-derives against these.
    pub(crate) fn held_releases(&self) -> Vec<ReleaseView> {
        self.held_tenant
            .iter()
            .filter_map(|r| {
                r.release
                    .as_ref()
                    .map(|f| (r.op_hash, f.export_id, f.content_digest, f.sources.clone()))
            })
            .collect()
    }

    /// A held claim's (kind, statement, sensitivity rank).
    pub(crate) fn claim_content(&self, h: &[u8; 32]) -> Option<(String, String, u8)> {
        self.held_tenant
            .iter()
            .find(|r| r.op_hash == *h)
            .and_then(|r| r.claim.clone())
    }

    /// Tenant `m.judge` (§11.1's judgment rows + §11.2 admission).
    /// Verb selection is row-driven: judge.full requires the OWNER
    /// class (§11.4); judge.safe requires direct-human evidence and
    /// an observation/episode target; retract/supersede additionally
    /// admit through a claim-authoring verb under the AUTHOR
    /// relation (supersede only on workflow spaces). Counting toward
    /// status is the SEPARATE §11.2 policy question — an admitted
    /// judgment may be recorded and never status-changing.
    fn admit_judge(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        // Resolve the citations first (the verb decision needs the
        // grant AND the target).
        let h = &op.header;
        if h.tenant != "memory" {
            return Err(Unimplemented(format!("tenant {}", h.tenant)));
        }
        let Proof::Dev { cap, .. } = h.proof else {
            return ok(Err(Verdict::Rejected("proof-arm", "reject-permanent")));
        };
        let Some(grant) = self.grants.iter().find(|g| g.h_grant == cap).cloned() else {
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        let body = &op.body;
        let bad = || Verdict::Rejected("body-invariant", "reject-permanent");
        let Some(verdict) = body.get("verdict").and_then(|v| v.as_text()) else {
            return ok(Err(bad()));
        };
        let Some(target) = body.get("target").and_then(|n| n.bytes_n::<32>()) else {
            return ok(Err(bad()));
        };
        let Some(target_rec) = self
            .held_tenant
            .iter()
            .find(|r| r.op_hash == target)
            .cloned()
        else {
            // The target claim may arrive later (D-199 spirit).
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        let Some((target_kind, _, _)) = &target_rec.claim else {
            return ok(Err(bad()));
        };
        let replacement = match verdict {
            "supersede" => match body.get("replacement").and_then(|n| n.bytes_n::<32>()) {
                Some(r) => {
                    if !self.held_tenant.iter().any(|x| x.op_hash == r) {
                        return ok(Err(Verdict::Pending(
                            "ref-unresolved",
                            "pending-dependency",
                        )));
                    }
                    Some(r)
                }
                None => return ok(Err(bad())),
            },
            "accept" | "dispute" | "retract" | "retire" => None,
            "raise_class" | "declassify" => {
                return Err(Unimplemented("classification judgment arms".into()))
            }
            _ => return ok(Err(bad())),
        };

        // Verb selection (§11.1 rows).
        let human = h.actor_kind == "human" && h.attested_by.is_none();
        let has = |v: &str| grant.verbs.iter().any(|g| g == v);
        let owner_class = human && has("judge.full");
        let safe_kind = matches!(target_kind.as_str(), "observation" | "episode");
        // The AUTHOR relation for the authoring-verb rows: principal
        // equality, or same lineage with human evidence (§11.2).
        let author = (h.writer_lineage == target_rec.lineage
            && h.actor_kind == target_rec.actor_kind
            && h.actor_id == target_rec.actor_id)
            || (h.writer_lineage == target_rec.lineage && human);
        let target_space_class = self
            .spaces
            .iter()
            .find(|sp| sp.space_id == target_rec.space)
            .map(|sp| sp.space_class.clone());
        let verb = match verdict {
            "accept" | "dispute" | "retire" => {
                if owner_class {
                    "judge.full"
                } else if has("judge.safe") && human && safe_kind {
                    "judge.safe"
                } else if has("judge.safe") || has("judge.full") {
                    // The verb exists; a row invariant fails
                    // (evidence / class / kind).
                    return ok(Err(bad()));
                } else {
                    return ok(Err(Verdict::Rejected("scope-op", "reject-permanent")));
                }
            }
            "retract" => {
                if owner_class {
                    "judge.full"
                } else if author && has("propose") {
                    "propose"
                } else if author && has("assert") {
                    "assert"
                } else if has("judge.full") || has("propose") || has("assert") {
                    return ok(Err(bad()));
                } else {
                    return ok(Err(Verdict::Rejected("scope-op", "reject-permanent")));
                }
            }
            "supersede" => {
                let in_workflow = target_space_class.as_deref() == Some("workflow");
                if owner_class {
                    "judge.full"
                } else if author && in_workflow && has("propose") {
                    "propose"
                } else if author && in_workflow && has("assert") {
                    "assert"
                } else if has("judge.full") || has("propose") || has("assert") {
                    return ok(Err(bad()));
                } else {
                    return ok(Err(Verdict::Rejected("scope-op", "reject-permanent")));
                }
            }
            _ => unreachable!("verdict vetted above"),
        };

        // The shared preamble under the selected verb.
        let grant = match self.tenant_preamble(op, verb)? {
            Ok(g) => g,
            Err(v) => return ok(Err(v)),
        };
        // Target in grant scope (zone/space axes against the CITING
        // grant; the kind axis binds the TARGET's kind).
        if grant.zone.is_some_and(|z| z != target_rec.zone) {
            return ok(Err(Verdict::Rejected("scope-zone", "reject-permanent")));
        }
        if grant
            .spaces
            .as_ref()
            .is_some_and(|sp| !sp.contains(&target_rec.space))
        {
            return ok(Err(Verdict::Rejected("scope-space", "reject-permanent")));
        }
        if grant
            .kinds
            .as_ref()
            .is_some_and(|ks| !ks.iter().any(|k| k == target_kind))
        {
            return ok(Err(Verdict::Rejected("scope-kind", "reject-permanent")));
        }

        // The cited polref must match the TARGET space's bound
        // policy (§13.3: a policy hash mismatch is `policy-missing`).
        let Some(pol) = body.get("policy") else {
            return ok(Err(bad()));
        };
        let (Some(pid), Some(phash)) = (
            pol.get("id").and_then(|n| n.as_text()),
            pol.get("hash").and_then(|n| n.bytes_n::<32>()),
        ) else {
            return ok(Err(bad()));
        };
        let Some(space) = self
            .spaces
            .iter()
            .find(|sp| sp.space_id == target_rec.space)
        else {
            return ok(Err(Verdict::Pending(
                "ref-unresolved",
                "pending-dependency",
            )));
        };
        if space.policy_id != pid || space.policy_hash != phash {
            return ok(Err(Verdict::Pending(
                "policy-missing",
                "pending-dependency",
            )));
        }

        self.record_tenant(
            op,
            &grant,
            None,
            None,
            None,
            Some(JudgeFacts {
                verdict: verdict.to_string(),
                target,
                replacement,
            }),
        );
        ok(Ok(()))
    }

    /// Does judgment `j` COUNT toward `target`'s status under the
    /// target space's bound policy (§11.2)? Admission already passed
    /// (j is held); this is the five-axis rule match.
    fn judgment_counts(&self, j: &HeldTenantOp, target: &HeldTenantOp) -> bool {
        let Some(jf) = &j.judge else { return false };
        let Some((target_kind, _, _)) = &target.claim else {
            return false;
        };
        let Some(space) = self.spaces.iter().find(|sp| sp.space_id == target.space) else {
            return false;
        };
        let Some(rules) = crate::policies::rules_for(&space.policy_id) else {
            return false;
        };
        let relation_holds = |rel: &str| -> bool {
            match rel {
                "any" => true,
                "self" => {
                    j.lineage == target.lineage
                        && j.actor_kind == target.actor_kind
                        && j.actor_id == target.actor_id
                }
                "author" => {
                    (j.lineage == target.lineage
                        && j.actor_kind == target.actor_kind
                        && j.actor_id == target.actor_id)
                        || (j.lineage == target.lineage && j.human_evidence)
                }
                _ => false,
            }
        };
        rules.iter().any(|r| {
            r.verdict == jf.verdict
                && r.kinds.is_none_or(|ks| ks.contains(&target_kind.as_str()))
                && r.space_classes
                    .is_none_or(|scs| scs.contains(&space.space_class.as_str()))
                && r.actor_classes.contains(&j.actor_class)
                && relation_holds(r.relation)
        })
    }

    /// The §11.2 status fold. `as_of` is carried for the temporal
    /// terms (none of the corpus claims carry validity windows yet —
    /// the parameter is threaded, unused). Cycle detection per rule 2
    /// (a supersession cycle derives `disputed`).
    pub(crate) fn claim_status(&self, target_hash: &[u8; 32], as_of: u64) -> Option<&'static str> {
        self.claim_status_inner(target_hash, as_of, &mut Vec::new())
    }

    // The `as_of` parameter is threaded for the temporal terms no
    // corpus claim exercises yet (valid_from/expires) — deliberately
    // recursion-only today.
    #[allow(clippy::only_used_in_recursion)]
    fn claim_status_inner(
        &self,
        target_hash: &[u8; 32],
        as_of: u64,
        visiting: &mut Vec<[u8; 32]>,
    ) -> Option<&'static str> {
        let target = self
            .held_tenant
            .iter()
            .find(|r| r.op_hash == *target_hash && r.claim.is_some())?;
        if !self.op_standing(target) {
            // A quarantined claim holds no status lane here; the
            // derived verdict already carries its state.
            return Some("candidate");
        }
        let judgments: Vec<&HeldTenantOp> = self
            .held_tenant
            .iter()
            .filter(|j| {
                j.judge.as_ref().is_some_and(|jf| jf.target == *target_hash)
                    && self.op_standing(j)
                    && self.judgment_counts(j, target)
            })
            .collect();
        // 1: retract/retire.
        if judgments.iter().any(|j| {
            matches!(
                j.judge.as_ref().expect("filtered").verdict.as_str(),
                "retract" | "retire"
            )
        }) {
            return Some("retired");
        }
        // 2: supersede with an ACCEPTED replacement; cycles dispute.
        for j in &judgments {
            let jf = j.judge.as_ref().expect("filtered");
            if jf.verdict != "supersede" {
                continue;
            }
            let Some(r) = jf.replacement else { continue };
            if visiting.contains(&r) || r == *target_hash {
                return Some("disputed");
            }
            visiting.push(*target_hash);
            let r_status = self.claim_status_inner(&r, as_of, visiting);
            visiting.pop();
            if r_status == Some("accepted") {
                return Some("superseded");
            }
        }
        // 3: an authorized dispute (ancestor-exempt accepts need
        // causal references, which no corpus fixture carries yet).
        if judgments
            .iter()
            .any(|j| j.judge.as_ref().expect("filtered").verdict == "dispute")
        {
            return Some("disputed");
        }
        // 4: accept.
        if judgments
            .iter()
            .any(|j| j.judge.as_ref().expect("filtered").verdict == "accept")
        {
            return Some("accepted");
        }
        // 5.
        Some("candidate")
    }

    /// Dispatch one operation.
    fn admit(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        // §7.5 (c): a control operation OUTSIDE the hosted-admissible
        // set rejects at the authority stage — the position/signature
        // stages still precede it (D-76 first-failing-stage; the
        // fixtures' inadmissible ops are admin-armed and validly
        // signed).
        if self.hosted_ceiling_active()
            && op.header.operation_type.starts_with("c.")
            && op.header.operation_type != "c.genesis"
            && !HOSTED_CTRL_ADMISSIBLE.contains(&op.header.operation_type)
        {
            if let Err(v) = self.ctrl_admin_preamble(op) {
                return ok(Err(v));
            }
            return ok(Err(Verdict::Rejected("hosted-ceiling", "reject-permanent")));
        }
        match op.header.operation_type {
            "c.genesis" => self.admit_genesis(op),
            "c.enroll" => self.admit_enroll(op),
            "c.grant" => self.admit_grant(op),
            "c.revoke_grant" => self.admit_revoke_grant(op),
            "c.revoke_device" => self.admit_revoke_device(op),
            "c.cutoff" => self.admit_cutoff(op),
            "c.cap_epoch_bump" => self.admit_cap_epoch_bump(op),
            "c.zone_policy" => self.admit_zone_policy(op),
            "c.kek_rotate" => self.admit_kek_rotate(op),
            "c.zone_create" => self.admit_zone_create(op),
            "c.space_create" => self.admit_space_create(op),
            "c.recovery_succession" => self.admit_recovery(op),
            "c.drill" => self.admit_drill(op),
            "m.claim" => self.admit_claim(op),
            "m.judge" => self.admit_judge(op),
            "m.export.release" => self.admit_release(op),
            "m.import.claim" => self.admit_import(op),
            "m.erase_request" => self.admit_erase_request(op),
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
    run_delivery_full(items, &BTreeMap::new(), order).map(|(run, _)| run)
}

/// [`run_delivery`] returning the final [`State`] too — the
/// export-import lane re-derives against held facts.
pub fn run_delivery_with_state(
    items: &BTreeMap<String, Vec<u8>>,
    order: &[String],
) -> Result<(Run, State), Unimplemented> {
    run_delivery_full(items, &BTreeMap::new(), order)
}

/// The full fold entry: `aux` = the vector's HELD context (§5.6
/// index, §4.7 receipts/leases) — installed before anything folds.
pub fn run_delivery_full(
    items: &BTreeMap<String, Vec<u8>>,
    aux: &BTreeMap<String, Vec<u8>>,
    order: &[String],
) -> Result<(Run, State), Unimplemented> {
    let mut state = State::default();
    state.install_aux(aux)?;
    let mut verdicts: BTreeMap<String, Verdict> = BTreeMap::new();
    let mut snapshots = Vec::new();
    // Pending queue in arrival order.
    let mut pending: Vec<String> = Vec::new();
    // name → op hash, for the derived-lane overlay.
    let mut hashes: BTreeMap<String, [u8; 32]> = BTreeMap::new();

    for name in order {
        let bytes = &items[name];
        if let Ok(op) = parse_op(bytes) {
            hashes.insert(name.clone(), op.op_hash());
        }
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
        // The derived lanes (§10.5): a held tenant op's fold verdict
        // is a projection of current state — recompute after every
        // delivery's fixpoint (retro-quarantine, claimant
        // re-derivation) and overlay.
        let derived = state.derived_tenant_verdicts()?;
        for (n, h) in &hashes {
            // A duplicate delivery is an edge fact about THAT
            // delivery — never overlaid by the shared op's fold state.
            if verdicts.get(n) == Some(&Verdict::Rejected("duplicate", "duplicate-idempotent")) {
                continue;
            }
            if let Some(v) = derived.get(h) {
                verdicts.insert(n.clone(), *v);
            }
            // Control ops re-classified by a freeze or a cut (§7.4).
            if let Some(v) = state.ctrl_overlay.get(h) {
                verdicts.insert(n.clone(), *v);
            }
        }
        snapshots.push(verdicts.clone());
    }
    Ok((
        Run {
            final_verdicts: verdicts,
            snapshots,
        },
        state,
    ))
}

pub(crate) fn classify(state: &mut State, bytes: &[u8]) -> Result<Verdict, Unimplemented> {
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
    let replay_key = (
        op.header.zone_id,
        op.header.writer_lineage,
        op.header.request_id,
    );
    if let Some(&seen) = state.request_seen.get(&replay_key) {
        return Ok(if seen == op.op_hash() {
            Verdict::Rejected("duplicate", "duplicate-idempotent")
        } else {
            Verdict::Rejected("request-fork", "reject-permanent")
        });
    }
    // The control fork/freeze gate (C2/C5) — recovery carries its
    // own placement rules (the §7.4 precedence exception).
    if op.header.operation_type.starts_with("c.")
        && op.header.operation_type != "c.recovery_succession"
    {
        if let Some(v) = state.ctrl_fork_gate(&op)? {
            return Ok(v);
        }
    }
    state.admit(&op).map(|r| match r {
        Ok(()) => {
            state.request_seen.insert(replay_key, op.op_hash());
            if op.header.operation_type.starts_with("c.") {
                state
                    .ctrl_log
                    .push((op.header.writer_sequence, bytes.to_vec()));
            }
            Verdict::Admitted
        }
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

    /// The full collision arc: m1 wins the replay key, the revocation
    /// freezes it (D-155), m2 collides against the frozen owner
    /// (D-161/D-169 — the trace row), and the C3′'s omission blanket
    /// kills m1's basis so the claimant fold re-derives m2 to owner
    /// (D-196) while m1 retro-quarantines under `cutoff`.
    #[test]
    fn collision_loser_reenters_when_the_winner_basis_dies() {
        let (items, _) = load("f11-collision-loser-reenters-on-winner-death.json");
        let order: Vec<String> = [
            "c1", "cz", "cs", "c2", "gf", "i1", "rel", "m1", "rg", "c3", "m2", "r",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let run = run_delivery(&items, &order).unwrap();
        // m1 is the admitted owner right up to the recovery.
        assert_eq!(run.snapshots[8]["m1"], Verdict::Admitted);
        // m2 at its own delivery: a claim against a FROZEN owner.
        assert_eq!(
            run.snapshots[10]["m2"],
            Verdict::Rejected("import-collision", "quarantine-reproposal")
        );
        // The C3′ flips both: the blanket cuts m1, m2 re-derives.
        assert_eq!(
            run.final_verdicts["m1"],
            Verdict::Rejected("cutoff", "quarantine-reproposal")
        );
        for k in [
            "c1", "cz", "cs", "c2", "gf", "i1", "rel", "rg", "c3", "m2", "r",
        ] {
            assert_eq!(run.final_verdicts[k], Verdict::Admitted, "{k}");
        }
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
