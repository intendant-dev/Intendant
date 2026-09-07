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
    /// §4.3 `budget: {max_ops, max_bytes}` — per-(grant, lineage)
    /// window accounting.
    budget: Option<(u64, u64)>,
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
    /// §11.4 derived actor class; `None` = a bare non-human
    /// unattested writer — NO class by the owner's D2 ruling
    /// (alternative (c)): recorded where authoring verbs admit it,
    /// counts toward no status rule.
    actor_class: Option<&'static str>,
    /// The §9.1/T5 basis when the op is deadline/lease-bearing (T4
    /// re-evaluation input).
    time: Option<TimeFacts>,
    /// The signed capability epoch — the §4.3 budget-window anchor.
    cap_epoch: u64,
    /// Canonical triple byte length (the §4.3 `max_bytes` charge;
    /// verb surcharges are unimplemented).
    op_len: u64,
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
/// A §4.3 budget-accounting key: (grant hash, lineage, window).
type BudgetKey = ([u8; 32], [u8; 16], u64);
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
    /// Accepted `m.erase_request`s: op hash → targets (the §5.4
    /// manifest-admission citation source).
    erase_requests: BTreeMap<[u8; 32], Vec<[u8; 32]>>,
    /// `item_addr`s already manifested by an accepted rotation —
    /// D-66 first-manifest-wins; re-appearance is an idempotent
    /// skip.
    manifested: Vec<[u8; 32]>,
    /// T4 (D-203-ratified compromise machinery): per signing-key
    /// compromise cutoffs — `key_id → accepted_through issuer_seq`,
    /// repeated cutoffs merged at the MINIMUM. Statements beyond the
    /// boundary are retro-disqualified as time evidence.
    receipt_cutoffs: BTreeMap<[u8; 32], u64>,
    /// Accepted `c.kek_rotate` ops: `H_op → (zone, new_epoch,
    /// control position)` — the `rotation_refs` linkage source.
    rotations: BTreeMap<[u8; 32], ([u8; 16], u64, u64)>,
    /// The control position of each device's LAST accepted wrap per
    /// zone — the D-71 post-last-wrap linkage bound (a stale
    /// rotation preceding a re-wrap excludes nothing).
    last_wrap_pos: BTreeMap<([u8; 16], [u8; 16]), u64>,
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
    /// §4.3/D-79 budget windows: (zone, capability epoch) → window
    /// ordinal. A bump opens a NEW window; a zone-policy advance
    /// carries the old one (re-arms nothing).
    zone_windows: BTreeMap<([u8; 16], u64), u64>,
    /// Accepted `m.audit` rows (§11.1/D-74): the partition registry.
    audit_rows: Vec<AuditRow>,
    /// D-130 same-coordinate fork evidence: equal `(zone, lineage,
    /// gen, seq)` with differing op hashes are NEVER ordered — every
    /// byte-variant seen at the coordinate (held or boundary-named)
    /// registers here, and the coordinate's suffix is inert until a
    /// committed boundary selects a variant.
    tenant_forks: BTreeMap<TenantCoord, Vec<[u8; 32]>>,
    /// D-130 committed selections: the FIRST committed boundary in
    /// control order naming one of a coordinate's variants selects
    /// it; a later boundary naming a different variant there is
    /// `body-invariant`.
    fork_selected: BTreeMap<TenantCoord, [u8; 32]>,
    /// D-202 evidence-history: operations whose `lease-stale` was
    /// ISSUED — terminal where issued; later timely evidence never
    /// revives them (convergence rides the writer's re-proposed op).
    /// Like fork evidence, an issue is a fact about this replica's
    /// evaluation history and persists on the real state.
    stale_issued: std::collections::BTreeSet<[u8; 32]>,
}

/// A tenant chain coordinate: (zone, lineage, gen, seq).
type TenantCoord = ([u8; 16], [u8; 16], u64, u64);

/// D-130 selections a committing boundary carries: (coordinate,
/// selected byte-variant).
type Selections = Vec<(TenantCoord, [u8; 32])>;

/// A validated frontierclose Head set: the `(gen, seq)` caps plus
/// any D-130 selections the boundary commits AT ITS TRANSITION.
type HeadsView = (Vec<(u64, u64)>, Selections);

/// One accepted audit row's partition facts (D-74/D-83).
#[derive(Debug, Clone)]
struct AuditRow {
    op_hash: [u8; 32],
    read_id: [u8; 16],
    principal_raw: Vec<u8>,
    scope_raw: Vec<u8>,
    index: u64,
    count: u64,
    result_ids: Vec<[u8; 32]>,
}

/// The retained §9.1/T5 basis of an admitted deadline- or
/// lease-bearing operation — the derived lane re-runs the SAME
/// predicates over it when compromise cutoffs land (T4
/// retro-disqualification; control-plane, distinct from the D-202
/// evidence-arrival stickiness).
#[derive(Debug, Clone)]
struct TimeFacts {
    signer_device: [u8; 16],
    deadlines: Vec<u64>,
    /// `(grant_id, max_age_ms)` when the citing grant is
    /// online-lease.
    lease: Option<([u8; 16], u64)>,
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
    /// The issuer-feed position (T4: compromise cutoffs
    /// retro-disqualify beyond it).
    issuer_seq: u64,
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
    /// The issuer-feed position (T4).
    issuer_seq: u64,
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

/// Exact key-SET equality (canonical order is the reader's proof).
fn keys_are_map(n: &Node, want: &[&str]) -> bool {
    n.map_keys().is_some_and(|mut k| {
        k.sort_unstable();
        let mut w = want.to_vec();
        w.sort_unstable();
        k == w
    })
}

/// App A.1 `cert` — required / optional key sets, verbatim.
const CERT_REQ: &[&str] = &[
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
    "revocation_id",
];
const CERT_OPT: &[&str] = &["evidence_media_type", "expiry_deadline_ms", "renews"];

/// App A.1 `grant` — required / optional key sets, verbatim.
const GRANT_REQ: &[&str] = &[
    "v",
    "plane_id",
    "grant_id",
    "subject_device",
    "tenants",
    "zone",
    "spaces",
    "ops",
    "class_ceiling",
    "online_lease",
    "issued_admin_epoch",
    "capability_epoch",
];
const GRANT_OPT: &[&str] = &[
    "lineage",
    "kinds",
    "can_declassify",
    "can_raise",
    "raise_quota",
    "flows",
    "budget",
    "max_age_ms",
    "expiry_deadline_ms",
];

/// App A.3 `kekwrap` — the exact key set.
const KEKWRAP_KEYS: &[&str] = &[
    "v",
    "plane_id",
    "zone_id",
    "epoch",
    "recipient_device",
    "recipient_kem_key",
    "kem",
    "enc",
    "ct",
];

/// App A.3 `zonepolicy` — required / optional key sets, verbatim.
const ZONEPOLICY_REQ: &[&str] = &[
    "v",
    "zone_id",
    "strictness",
    "deadline_fallback",
    "require_cert_deadlines",
];
const ZONEPOLICY_OPT: &[&str] = &["grant_epoch_slack", "time_witnesses", "connect_service_key"];

/// App A.3 `spacedef` — the exact key set.
const SPACEDEF_KEYS: &[&str] = &[
    "space_id",
    "zone_id",
    "name_hash",
    "space_class",
    "class_minimum",
    "status_policy",
];

/// The closed-CDDL key discipline (O3, the ff23f1cd review's F2):
/// every required key present, every present key required-or-
/// optional — unknown fields in a registry body reject exactly as
/// in headers.
fn keys_within(n: &Node, required: &[&str], optional: &[&str]) -> bool {
    let Some(keys) = n.map_keys() else {
        return false;
    };
    required.iter().all(|r| keys.contains(r))
        && keys
            .iter()
            .all(|k| required.contains(k) || optional.contains(k))
}

impl State {
    /// O5 replay consult (§11.1) — read-only; consumption happens at
    /// the ACCEPTING transition only (D-112). Byte-identical
    /// redelivery is the delivery-edge duplicate; differing bytes
    /// under a consumed request_id is `request-fork`.
    fn request_check(&self, op: &SignedOp) -> Option<Verdict> {
        let key = (
            op.header.zone_id,
            op.header.writer_lineage,
            op.header.request_id,
        );
        match self.request_seen.get(&key) {
            Some(&seen) if seen == op.op_hash() => {
                Some(Verdict::Rejected("duplicate", "duplicate-idempotent"))
            }
            Some(_) => Some(Verdict::Rejected("request-fork", "reject-permanent")),
            None => None,
        }
    }

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
            budget: grant_node
                .get("budget")
                .and_then(|b| Some((b.get("max_ops")?.as_uint()?, b.get("max_bytes")?.as_uint()?))),
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
    /// a re-wrap supersedes by `(zone, epoch, device)`). Runs inside
    /// the accepting op's effect block, BEFORE its chain advance, so
    /// `ctrl_next_seq` is that op's own position — recorded as the
    /// device's last-wrap bound (D-71 linkage).
    fn record_wrap(&mut self, zone: [u8; 16], epoch: u64, device: [u8; 16]) {
        let set = self.wrap_sets.entry((zone, epoch)).or_default();
        if !set.contains(&device) {
            set.push(device);
        }
        self.last_wrap_pos
            .insert((zone, device), self.ctrl_next_seq);
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
        self.zone_windows
            .insert((zone_id.unwrap_or_default(), 1), 0);
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
        let mut selections = Vec::new();
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
                Ok((h, sels)) => {
                    caps = h;
                    selections = sels;
                }
                Err(v) => return ok(Err(v)),
            }
        } else if cutoff.is_some() {
            return Err(Unimplemented(
                "cutoff on a read-only grant revocation".into(),
            ));
        }

        // Accept: the boundary commits — D-130 selections first.
        self.apply_selections(&selections);
        // Deactivate the grant, install the revoke boundary
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
    /// resolving each against the HELD chain: an unheld coordinate
    /// pends `ref-unresolved`; a named hash genuinely held at the
    /// coordinate (the accepted op or a registered fork variant)
    /// resolves, committing a D-130 selection where fork evidence
    /// exists; a named hash whose BYTES are not held pends
    /// `ref-unresolved` until they arrive (§7.1's referenced-Head
    /// lifecycle — the exact-reference rule, the criterion-12 F2
    /// repair); a hash conflicting a committed selection is
    /// `(body-invariant, reject-permanent)`.
    fn parse_heads(
        &self,
        cn: &Node,
        zone: [u8; 16],
        lineage: [u8; 16],
    ) -> Result<Result<HeadsView, Verdict>, Unimplemented> {
        let Some(heads) = cn.get("heads").and_then(|h| h.as_array()) else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        let mut caps = Vec::new();
        let mut selections: Vec<(TenantCoord, [u8; 32])> = Vec::new();
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
            let coord = (zone, lineage, gen, seq);
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
                Some(r) => {
                    // D-130 under the exact-reference rule (§7.1's
                    // referenced-Head lifecycle; the criterion-12 F2
                    // repair): a boundary selects only a byte-variant
                    // genuinely HELD at the coordinate — the accepted
                    // op or a registered fork variant. A named hash
                    // whose bytes are NOT held is a pending reference
                    // (`ref-unresolved` until the exact bytes
                    // arrive), never a selection — the v0.5.9
                    // differing-hash REJECTION stays superseded
                    // (D-93's rider); `body-invariant` only against a
                    // PRIOR committed selection of a different
                    // variant (that conflict is with the committed
                    // selection fact, independent of byte content).
                    match self.fork_selected.get(&coord) {
                        Some(&sel) if sel != hop => {
                            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")))
                        }
                        Some(_) => {}
                        None => {
                            let held_variant = self
                                .tenant_forks
                                .get(&coord)
                                .is_some_and(|v| v.contains(&hop));
                            if r.op_hash == hop {
                                // The held accepted op — a selection
                                // commits only where fork evidence
                                // exists to resolve.
                                if self.tenant_forks.contains_key(&coord) {
                                    selections.push((coord, hop));
                                }
                            } else if held_variant {
                                selections.push((coord, hop));
                            } else {
                                return ok(Err(Verdict::Pending(
                                    "ref-unresolved",
                                    "pending-dependency",
                                )));
                            }
                        }
                    }
                    caps.push((gen, seq));
                }
            }
        }
        ok(Ok((caps, selections)))
    }

    /// Is `(gen, seq)` at or below one of the carried caps?
    fn at_or_below(caps: &[(u64, u64)], gen: u64, seq: u64) -> bool {
        caps.iter().any(|&(g, s)| g == gen && seq <= s)
    }

    /// Register D-130 fork evidence for an operation whose staged
    /// admission returned the fork pair — the preamble's chain stage
    /// fires only AFTER the validity stages (cert, sig, body, actor,
    /// proof scopes) pass, so reaching it proves evidence-worthiness
    /// (D-99: a failed earlier stage keeps its own outcome and exerts
    /// no precedence). Runs on the REAL state: fork evidence
    /// persists like a C2 freeze while the failed op's other effects
    /// roll back with its clone.
    fn register_tenant_fork(&mut self, op: &SignedOp) {
        let h = &op.header;
        let coord = (h.zone_id, h.writer_lineage, h.writer_gen, h.writer_sequence);
        let incumbent = self
            .held_tenant
            .iter()
            .find(|r| {
                r.zone == h.zone_id
                    && r.lineage == h.writer_lineage
                    && r.gen == h.writer_gen
                    && r.seq == h.writer_sequence
            })
            .map(|r| r.op_hash);
        let entry = self.tenant_forks.entry(coord).or_default();
        for v in [incumbent, Some(op.op_hash())].into_iter().flatten() {
            if !entry.contains(&v) {
                entry.push(v);
            }
        }
    }

    /// The derived D-130 lane for a HELD op: `Some(verdict)` when its
    /// coordinate (or an ancestor coordinate on the same chain) holds
    /// unresolved fork evidence, or when a committed selection chose
    /// a different variant (the losing branch quarantines).
    fn fork_verdict(&self, rec: &HeldTenantOp) -> Option<Verdict> {
        for &(z, l, g, s) in self.tenant_forks.keys() {
            if z != rec.zone || l != rec.lineage || g != rec.gen || s > rec.seq {
                continue;
            }
            match self.fork_selected.get(&(z, l, g, s)) {
                None => return Some(Verdict::Rejected("fork", "freeze-writer")),
                Some(&sel) => {
                    if s == rec.seq && sel != rec.op_hash {
                        // The losing branch quarantines (D-130).
                        return Some(Verdict::Rejected("cutoff", "quarantine-reproposal"));
                    }
                }
            }
        }
        None
    }

    /// Apply a committed boundary's D-130 selections (at the
    /// boundary's TRANSITION only — a pending compound's reservation
    /// never selects).
    fn apply_selections(&mut self, sels: &[(TenantCoord, [u8; 32])]) {
        for &(coord, hop) in sels {
            let entry = self.tenant_forks.entry(coord).or_default();
            if !entry.contains(&hop) {
                entry.push(hop);
            }
            self.fork_selected.entry(coord).or_insert(hop);
        }
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
            } else if let Some(fv) = self.fork_verdict(rec) {
                // D-130: an unresolved same-coordinate fork freezes
                // the coordinate and its suffix; a committed
                // selection quarantines the losing branch.
                fv
            } else if !self.op_standing(rec) {
                Verdict::Rejected("cutoff", "quarantine-reproposal")
            } else if let Some(tv) = self.time_requalify(rec) {
                tv
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
        // §4.3 budgets — a pure fold in canonical (gen, seq) order
        // over the ACCEPTED set per (grant, lineage, window): only
        // accepted operations consume (D-94), and everything past
        // the line displaces to (budget, quarantine-reproposal) —
        // arrival order is immaterial by construction (D-86).
        let mut groups: BTreeMap<BudgetKey, Vec<&HeldTenantOp>> = BTreeMap::new();
        for rec in &self.held_tenant {
            if out.get(&rec.op_hash) != Some(&Verdict::Admitted) {
                continue;
            }
            let Some(grant) = self.grants.iter().find(|g| g.h_grant == rec.cited_grant) else {
                continue;
            };
            if grant.budget.is_none() {
                continue;
            }
            let window = self
                .zone_windows
                .get(&(rec.zone, rec.cap_epoch))
                .copied()
                .unwrap_or(0);
            groups
                .entry((rec.cited_grant, rec.lineage, window))
                .or_default()
                .push(rec);
        }
        for ((grant_hash, _, _), mut recs) in groups {
            let (max_ops, max_bytes) = self
                .grants
                .iter()
                .find(|g| g.h_grant == grant_hash)
                .and_then(|g| g.budget)
                .expect("grouped on a budgeted grant");
            recs.sort_by_key(|r| (r.gen, r.seq));
            let (mut ops, mut bytes) = (0u64, 0u64);
            for rec in recs {
                ops += 1;
                bytes += rec.op_len;
                if ops > max_ops || bytes > max_bytes {
                    out.insert(
                        rec.op_hash,
                        Verdict::Rejected("budget", "quarantine-reproposal"),
                    );
                }
            }
        }
        Ok(out)
    }

    /// Parse a compound's `cutoffs` into `(zone, lineage)` pairs
    /// with D-143 exactness: carried heads validate against the held
    /// chain (unheld pends; a differing hash at the coordinate is
    /// D-130 fork evidence the COMMITTING boundary selects), and
    /// EMPTY heads are legal only for a lineage with NO accepted ops
    /// (a lineage with history must commit its boundary). Returned
    /// selections apply at the compound's COMPLETION transition —
    /// never at reservation (a pending compound has not committed).
    /// The heads' beyond-boundary retirement effect on tenant
    /// history rides the Gate-B retirement sagas (D-203-recorded
    /// deferral) — this slice validates the commitment.
    fn compound_cutoffs(
        &self,
        body: &Node,
    ) -> Result<Result<(Vec<ZoneLineage>, Selections), Verdict>, Unimplemented> {
        let mut out = Vec::new();
        let mut selections = Vec::new();
        let Some(cs) = body.get("cutoffs").and_then(|c| c.as_array()) else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        for cn in cs {
            let (Some(z), Some(l)) = (b16_field(cn, "zone_id"), b16_field(cn, "lineage")) else {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            };
            let caps = match self.parse_heads(cn, z, l)? {
                Ok((c, sels)) => {
                    selections.extend(sels);
                    c
                }
                Err(v) => return ok(Err(v)),
            };
            let has_history = self
                .tenant_chains
                .keys()
                .any(|(cz, cl, _)| *cz == z && *cl == l);
            if caps.is_empty() && has_history {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            }
            out.push((z, l));
        }
        ok(Ok((out, selections)))
    }

    /// Re-read a reserved compound's `rotation_refs` (bytes were
    /// validated at reservation).
    fn compound_rotation_refs(body: &Node) -> Vec<[u8; 32]> {
        body.get("rotation_refs")
            .and_then(|r| r.as_array())
            .map(|a| a.iter().filter_map(|n| n.bytes_n::<32>()).collect())
            .unwrap_or_default()
    }

    /// Parse a compound's `receipt_cutoffs` (T4, compromise mode):
    /// entries `{key_id, through, head_hash}`, `head_hash` all-zero
    /// exactly when `through = 0` (D-87); repeated cutoffs for one
    /// key merge at the MINIMUM. The `head_hash` commitment for
    /// `through > 0` (the stmt_id of statement #through) is
    /// verifiable only against a held issuer feed — the fold records
    /// the commitment; feed-side verification is audit/registry
    /// territory, documented in the Gate-A audit.
    fn compound_receipt_cutoffs(
        body: &Node,
        compromise: bool,
    ) -> Result<Vec<([u8; 32], u64)>, Verdict> {
        let bad = Verdict::Rejected("body-invariant", "reject-permanent");
        let node = body.get("receipt_cutoffs");
        if !compromise {
            // Exclude never carries the field (§7.1: compromise
            // ADDITIONALLY carries it).
            return if node.is_some() {
                Err(bad)
            } else {
                Ok(Vec::new())
            };
        }
        let Some(node) = node else {
            // A compromise may omit the field (CDDL `?`): the
            // cert/grant/cutoff effects fire, no feed is cut.
            return Ok(Vec::new());
        };
        let Some(entries) = node.as_array() else {
            return Err(bad);
        };
        if entries.is_empty() {
            return Err(bad); // CDDL `[+ …]`
        }
        let mut out: Vec<([u8; 32], u64)> = Vec::new();
        for e in entries {
            if !keys_are_map(e, &["key_id", "through", "head_hash"]) {
                return Err(bad);
            }
            let (Some(kid), Some(through), Some(head)) = (
                e.get("key_id").and_then(|n| n.bytes_n::<32>()),
                e.get("through").and_then(|n| n.as_uint()),
                e.get("head_hash").and_then(|n| n.bytes_n::<32>()),
            ) else {
                return Err(bad);
            };
            if through == 0 && head != [0u8; 32] {
                return Err(bad); // D-87
            }
            out.push((kid, through));
        }
        Ok(out)
    }

    /// Evaluate the one completion law (D-180/D-186) at the current
    /// position and, when it holds, apply the compound's effects: the
    /// certificates cease HERE (D-195), grant revocation is derived
    /// (D-85), and a compromise's receipt cutoffs land (T4 —
    /// statements beyond them retro-disqualify, min-merged).
    /// Incomplete → `ref-unresolved` (awaiting completing
    /// exclusions/cutoffs).
    fn try_complete_compound(
        &mut self,
        oh: [u8; 32],
        rid: [u8; 16],
        cutoffs: &[ZoneLineage],
        receipt_cutoffs: &[([u8; 32], u64)],
        rotation_refs: &[[u8; 32]],
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
        // rotation_refs (typed linkage, D-71): each reference must
        // resolve to an ACCEPTED rotation that is a valid
        // post-last-wrap EXCLUSION of the target — accepted strictly
        // after the target's last accepted wrap for its zone, with
        // the target outside the new epoch's recipient set. Unheld
        // pends the compound (verifiable-when-held); held-invalid is
        // body-invariant. References never discharge coverage
        // (D-165/D-180: completion stays state-derived below). A
        // reserved compound whose late-arriving reference proves
        // invalid dies here with its position consumed — no vector
        // pins that residue yet.
        for r in rotation_refs {
            let Some(&(zone, new_epoch, pos)) = self.rotations.get(r) else {
                return Err(pend);
            };
            for d in &targets {
                if self
                    .wrap_sets
                    .get(&(zone, new_epoch))
                    .is_some_and(|set| set.contains(d))
                {
                    return Err(Verdict::Rejected("body-invariant", "reject-permanent"));
                }
                if self
                    .last_wrap_pos
                    .get(&(zone, *d))
                    .is_some_and(|&wp| wp >= pos)
                {
                    // Stale: a rotation preceding a re-wrap excludes
                    // nothing.
                    return Err(Verdict::Rejected("body-invariant", "reject-permanent"));
                }
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
        for &(kid, through) in receipt_cutoffs {
            let e = self.receipt_cutoffs.entry(kid).or_insert(through);
            *e = (*e).min(through);
        }
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
            let (cutoffs, selections) = match self.compound_cutoffs(&op.body)? {
                Ok(c) => c,
                Err(v) => return ok(Err(v)),
            };
            let compromise = op.body.get("mode").and_then(|m| m.as_text()) == Some("compromise");
            let rcuts = match Self::compound_receipt_cutoffs(&op.body, compromise) {
                Ok(r) => r,
                Err(v) => return ok(Err(v)),
            };
            let refs = Self::compound_rotation_refs(&op.body);
            let out = self.try_complete_compound(oh, rid, &cutoffs, &rcuts, &refs);
            if out.is_ok() {
                // The compound COMPLETED — its boundary commits, and
                // with it any D-130 selections (never at reservation).
                self.apply_selections(&selections);
            }
            if matches!(out, Err(Verdict::Rejected(..))) {
                // A held-invalid reference kills the reserved
                // compound; the reservation releases (the position
                // stays consumed).
                self.pending_compounds.remove(&oh);
            }
            return ok(out);
        }
        if let Err(v) = self.ctrl_admin_preamble(op) {
            return ok(Err(v));
        }
        let body = &op.body;
        let compromise = match body.get("mode").and_then(|m| m.as_text()) {
            Some("exclude") => false,
            Some("compromise") => true,
            _ => return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent"))),
        };
        let receipt_cuts = match Self::compound_receipt_cutoffs(body, compromise) {
            Ok(r) => r,
            Err(v) => return ok(Err(v)),
        };
        let Some(rid) = b16_field(body, "revocation_id") else {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        };
        // At most one live compound per revocation_id; a completed
        // target has no live certificate left to revoke.
        if self.pending_compounds.values().any(|r| *r == rid) || self.revoked_ids.contains(&rid) {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
        }
        // rotation_refs are typed linkage, never coverage (empty is
        // legal on a trusted plane: completion is state-derived) —
        // MANDATORY on hosted planes (§7.5). Structure here;
        // resolution + the D-71 post-last-wrap exclusion predicate
        // ride completion (an unheld reference pends the compound,
        // verifiable-when-held).
        let ref_hashes = match body.get("rotation_refs").and_then(|r| r.as_array()) {
            None => return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent"))),
            Some(a) => {
                if a.len() > 64 {
                    // E8.
                    return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
                }
                let mut out = Vec::new();
                for r in a {
                    let Some(h) = r.bytes_n::<32>() else {
                        return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
                    };
                    out.push(h);
                }
                out
            }
        };
        if self.provenance.as_deref() == Some("hosted") && ref_hashes.is_empty() {
            return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
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
        let (cutoffs, selections) = match self.compound_cutoffs(body)? {
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
        }
        // Reserve the position, then evaluate (the compound may
        // complete immediately).
        self.ctrl_next_seq += 1;
        self.ctrl_head = oh;
        self.pending_compounds.insert(oh, rid);
        let out = self.try_complete_compound(oh, rid, &cutoffs, &receipt_cuts, &ref_hashes);
        if out.is_ok() {
            // Immediate completion commits the boundary's selections;
            // a mere reservation selects nothing (D-130).
            self.apply_selections(&selections);
        }
        if matches!(out, Err(Verdict::Rejected(..))) {
            self.pending_compounds.remove(&oh);
        }
        ok(out)
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
        let mut selections = Vec::new();
        let closes = match body.get("closes").and_then(|c| c.as_array()) {
            Some(a) if !a.is_empty() => a,
            _ => return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent"))),
        };
        let mut staged: Vec<ZoneLineage> = Vec::new();
        for cn in closes {
            let (Some(z), Some(l)) = (b16_field(cn, "zone_id"), b16_field(cn, "lineage")) else {
                return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
            };
            // Carried heads validate per D-93 (unheld pends,
            // mismatch rejects); their consumption semantics ride
            // the Gate-B staging sagas.
            match self.parse_heads(cn, z, l)? {
                Ok((_caps, sels)) => {
                    selections.extend(sels);
                    staged.push((z, l));
                }
                Err(v) => return ok(Err(v)),
            }
        }

        // Accept: the stages exist from acceptance on (D-160), inert.
        self.apply_selections(&selections);
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
        let mut selections = Vec::new();
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
                match self.parse_heads(cn, cz, cl)? {
                    Ok((_caps, sels)) => {
                        selections.extend(sels);
                        entries.push(cl);
                    }
                    Err(v) => return ok(Err(v)),
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

        // Accept: the boundary commits — D-130 selections first.
        self.apply_selections(&selections);
        // Advance the capability epoch; the consuming advance
        // spends EVERY unconsumed stage for this zone (D-153 one-shot
        // — a prior advance's materialized entries never satisfy
        // later coverage). Budget-window state (D-79) has no consumer
        // in the engine yet.
        self.cap_epochs.insert(zone, cur + 1);
        let w = self.zone_windows.get(&(zone, cur)).copied().unwrap_or(0);
        self.zone_windows.insert((zone, cur + 1), w + 1);
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
        let mut selections = Vec::new();
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
                match self.parse_heads(cn, cz, cl)? {
                    Ok((_caps, sels)) => {
                        selections.extend(sels);
                        entries.push(cl);
                    }
                    Err(v) => return ok(Err(v)),
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

        // Accept: the boundary commits — D-130 selections first;
        // advance the epoch, consume the zone's stages one-shot, and
        // anchor the NEW policy at the new epoch.
        self.apply_selections(&selections);
        self.cap_epochs.insert(zone, cur + 1);
        // D-79: a policy advance re-arms NO budget window.
        let w = self.zone_windows.get(&(zone, cur)).copied().unwrap_or(0);
        self.zone_windows.insert((zone, cur + 1), w);
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
        // The typed erase manifest (§5.4, D-66 — the P1 profile's
        // first implement-before-Gate-A mechanism): entries
        // `{item_addr, erase_op, target_op}`, an E7 set keyed and
        // sorted by `item_addr`, at most 128 (E8). The PORTABLE
        // admission checks: `erase_op` cites an ACCEPTED
        // `m.erase_request` (unheld → pends; held-but-not-a-request
        // → body-invariant) and `target_op` is a member of that
        // request's `targets`. The `item_addr ↔ target_op` binding
        // is author-attested (verifiable only by zone-key index
        // rebuild, §5.6) and the D-198 nonterminal-journal
        // eligibility is a LOCAL storage invariant — neither is a
        // portable admission predicate. Cross-rotation re-appearance
        // of a manifested `item_addr` is an idempotent skip (D-66):
        // the entry re-admits, its effect does not repeat.
        let mut manifest_effects: Vec<([u8; 32], [u8; 32])> = Vec::new();
        match body.get("erase_manifest").and_then(|m| m.as_array()) {
            None => return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent"))),
            Some(entries) => {
                if entries.len() > 128 {
                    return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
                }
                let mut prev_addr: Option<[u8; 32]> = None;
                for e in entries {
                    if !keys_are_map(e, &["item_addr", "erase_op", "target_op"]) {
                        return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
                    }
                    let (Some(addr), Some(erase_op), Some(target_op)) = (
                        e.get("item_addr").and_then(|v| v.bytes_n::<32>()),
                        e.get("erase_op").and_then(|v| v.bytes_n::<32>()),
                        e.get("target_op").and_then(|v| v.bytes_n::<32>()),
                    ) else {
                        return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
                    };
                    if prev_addr.is_some_and(|p| p >= addr) {
                        return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
                    }
                    prev_addr = Some(addr);
                    let Some(targets) = self.erase_requests.get(&erase_op) else {
                        if self.holds_op(&erase_op) {
                            // Held but not an accepted erase request.
                            return ok(Err(Verdict::Rejected(
                                "body-invariant",
                                "reject-permanent",
                            )));
                        }
                        return ok(Err(Verdict::Pending(
                            "ref-unresolved",
                            "pending-dependency",
                        )));
                    };
                    if !targets.contains(&target_op) {
                        return ok(Err(Verdict::Rejected("body-invariant", "reject-permanent")));
                    }
                    manifest_effects.push((addr, target_op));
                }
            }
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
        self.rotations
            .insert(op.op_hash(), (zone, cur + 1, self.ctrl_next_seq));
        // Manifest effects (D-66 first-manifest-wins): a fresh
        // item_addr consumes its target's erase-queue entry; a
        // re-appearing one skips idempotently.
        for (addr, target_op) in manifest_effects {
            if self.manifested.contains(&addr) {
                continue;
            }
            self.manifested.push(addr);
            self.erase_queue.retain(|t| *t != target_op);
        }
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
        self.zone_windows.insert((zone_id, 1), 0);
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
        let mut selections = Vec::new();
        let Some(cutoffs) = body.get("tenant_cutoffs").and_then(|c| c.as_array()) else {
            return ok(Err(bad()));
        };
        let mut named = Vec::new();
        for cn in cutoffs {
            let (Some(cz), Some(cl)) = (b16_field(cn, "zone_id"), b16_field(cn, "lineage")) else {
                return ok(Err(bad()));
            };
            match self.parse_heads(cn, cz, cl)? {
                Ok((caps, sels)) => {
                    selections.extend(sels);
                    named.push(TenantBoundary {
                        zone: cz,
                        lineage: cl,
                        selector_grant: None,
                        caps,
                    });
                }
                Err(v) => return ok(Err(v)),
            }
        }

        // Accept: the recovery's named boundaries commit — D-130
        // selections first.
        self.apply_selections(&selections);
        // A below-head base first cuts the branch above it:
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

        // O5 replay (§11.1), consulted AFTER the validity stages:
        // request-ID consumption is scoped to accepted operations, so
        // a signature- or scope-invalid reuse keeps its own outcome.
        if let Some(v) = self.request_check(op) {
            return ok(Err(v));
        }

        // Chain: within (zone, lineage, gen), dense from 1.
        let key = (h.zone_id, h.writer_lineage, h.writer_gen);
        let (expect_seq, head) = self
            .tenant_chains
            .get(&key)
            .copied()
            .unwrap_or((1, [0u8; 32]));
        if h.writer_gen != 1 {
            // The generation machine is fail-closed in the ratified
            // P1 v1 profile: second generations quarantine
            // `lineage-gen` (revivable if a later profile implements
            // them — the D-140 below-bound reading).
            return ok(Err(Verdict::Rejected(
                "lineage-gen",
                "quarantine-reproposal",
            )));
        }
        match h.writer_sequence.cmp(&expect_seq) {
            std::cmp::Ordering::Less => {
                // An occupied coordinate with differing bytes is
                // D-130 fork evidence — never ordered, both variants
                // inert until a committed boundary selects one
                // (classify registers the evidence on the real
                // state). A byte-variant a boundary already SELECTED
                // re-arriving here would be the revival lane — no
                // vector pins it yet.
                let coord = (h.zone_id, h.writer_lineage, h.writer_gen, h.writer_sequence);
                if self.fork_selected.get(&coord) == Some(&op.op_hash()) {
                    return Err(Unimplemented("D-130 selected-variant revival".into()));
                }
                // The self-evidence exception (D-205, completing
                // D-204): a variant whose OWN held evidence already
                // classifies it lease-stale takes that sticky
                // terminal classification and registers NO fork
                // evidence — D-112's no-precedence rule extended to
                // fork registration: the condemned original never
                // freezes the writer's re-proposed convergence
                // carrier, so the late-first class converges on
                // EVERY relative delivery order of original and
                // re-proposal (the ff23f1cd review's F1 trace). An
                // evidence-less or timely-evidenced variant stays
                // fork evidence (the timely-first both-freeze world
                // is unchanged).
                if grant.online_lease {
                    if let Some(v @ Verdict::Rejected("lease-stale", _)) =
                        self.lease_self_verdict(op, &held_cert, &grant)
                    {
                        return ok(Err(v));
                    }
                }
                return ok(Err(Verdict::Rejected("fork", "freeze-writer")));
            }
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
                // (D-202 stickiness is consulted at the TOP of
                // classify — the memoized terminal verdict answers
                // before any pipeline stage.)
                if let Some(v) = self.lease_self_verdict(op, &held_cert, &grant) {
                    return ok(Err(v));
                }
            }
        }
        // The body stage opens with the registry-row consult, keyed
        // by ALL THREE coordinates — (tenant, operation_type,
        // operation_version), the ff23f1cd review's F3: an
        // unsupported semantic version rejects before any
        // arm-specific body reading (every v1 registry row is
        // operation_version 1).
        if op.header.operation_version != 1 {
            return ok(Err(Verdict::Rejected(
                "unknown-version",
                "reject-permanent",
            )));
        }
        ok(Ok(grant))
    }

    /// The T5 lease classification of `op` on currently HELD
    /// evidence — the one evaluator both consumers share (derive,
    /// don't mirror): the time stage proper, and the D-205
    /// self-evidence consult at an occupied coordinate. `None` =
    /// in-window (or the grant is not lease-bound); `lease-missing`
    /// pends where no window or no observation is held; a held
    /// qualified observation OUTSIDE every valid window is staleness
    /// on the held evidence — the owner's D5 ruling (D-202,
    /// alternative (ii)): STICKY, terminal where issued, convergence
    /// riding the writer's re-proposed op (exactly the
    /// quarantine-reproposal disposition).
    fn lease_self_verdict(
        &self,
        op: &SignedOp,
        held_cert: &HeldCert,
        grant: &HeldGrant,
    ) -> Option<Verdict> {
        if !grant.online_lease {
            return None;
        }
        let accepts = self.qualified_accepts(op, held_cert);
        let max_age = grant.max_age_ms.unwrap_or(0);
        let windows = self.qualified_lease_windows(op, held_cert, grant, max_age);
        if windows.is_empty() {
            return Some(Verdict::Pending("lease-missing", "pending-dependency"));
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
                return Some(Verdict::Pending("lease-missing", "pending-dependency"));
            }
            return Some(Verdict::Rejected("lease-stale", "quarantine-reproposal"));
        }
        None
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
        self.qualified_accepts_at(
            op.header.zone_id,
            op.header.capability_epoch,
            op.op_hash(),
            signer.device_id,
        )
    }

    /// The T2/T4 accept-qualification predicate over RETAINED facts
    /// (the derived lane re-runs it for held operations when
    /// compromise cutoffs land — T4 retro-disqualification).
    fn qualified_accepts_at(
        &self,
        zone: [u8; 16],
        cap_epoch: u64,
        op_hash: [u8; 32],
        signer_device: [u8; 16],
    ) -> Vec<u64> {
        let witnesses = self.witnesses_at(zone, cap_epoch);
        self.receipts
            .iter()
            .filter_map(|r| {
                if Some(r.plane) != self.plane_id || r.zone != zone {
                    return None;
                }
                if self.item_index.get(&r.subject) != Some(&op_hash) {
                    return None;
                }
                let cert = self.certs.iter().find(|c| c.h_cert == r.issuer_cert)?;
                if cert.device_id == signer_device {
                    return None; // T2: a signer never receipts itself
                }
                if !witnesses.contains(&cert.device_id) {
                    return None;
                }
                // T4: a compromise cutoff retro-disqualifies the
                // issuer's statements beyond `through`.
                let kid = domains::key_id("ed25519", &cert.sig_pk);
                if self
                    .receipt_cutoffs
                    .get(&kid)
                    .is_some_and(|&thr| r.issuer_seq > thr)
                {
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
        self.qualified_lease_windows_at(
            op.header.zone_id,
            op.header.capability_epoch,
            op.header.writer_lineage,
            grant.grant_id,
            max_age,
            signer.device_id,
        )
    }

    /// T4 re-evaluation for a HELD deadline/lease-bearing op: the
    /// §9.1/T5 predicates over the retained basis under CURRENT
    /// compromise cutoffs. `None` = still qualified. A receipt an
    /// accepted compromise cut no longer qualifies — control-plane
    /// retro-disqualification (D-138 re-derivation), distinct from
    /// the D-202 ruling, which makes `lease-stale` sticky against
    /// new EVIDENCE only.
    fn time_requalify(&self, rec: &HeldTenantOp) -> Option<Verdict> {
        let tf = rec.time.as_ref()?;
        let accepts =
            self.qualified_accepts_at(rec.zone, rec.cap_epoch, rec.op_hash, tf.signer_device);
        for &d in &tf.deadlines {
            if !accepts.iter().any(|&seen| seen <= d) {
                return Some(Verdict::Pending(
                    "deadline-unreceipted",
                    "pending-dependency",
                ));
            }
        }
        if let Some((grant_id, max_age)) = tf.lease {
            let windows = self.qualified_lease_windows_at(
                rec.zone,
                rec.cap_epoch,
                rec.lineage,
                grant_id,
                max_age,
                tf.signer_device,
            );
            if windows.is_empty() {
                return Some(Verdict::Pending("lease-missing", "pending-dependency"));
            }
            const SKEW_MS: u64 = 300_000;
            let in_window = windows
                .iter()
                .any(|&(i, e)| accepts.iter().any(|&s| s >= i && s <= e + SKEW_MS));
            if !in_window {
                return Some(if accepts.is_empty() {
                    Verdict::Pending("lease-missing", "pending-dependency")
                } else {
                    Verdict::Rejected("lease-stale", "quarantine-reproposal")
                });
            }
        }
        None
    }

    /// The lease twin of [`Self::qualified_accepts_at`].
    fn qualified_lease_windows_at(
        &self,
        zone: [u8; 16],
        cap_epoch: u64,
        lineage: [u8; 16],
        grant_id: [u8; 16],
        max_age: u64,
        signer_device: [u8; 16],
    ) -> Vec<(u64, u64)> {
        let witnesses = self.witnesses_at(zone, cap_epoch);
        self.leases
            .iter()
            .filter_map(|l| {
                if Some(l.plane) != self.plane_id || l.zone != zone {
                    return None;
                }
                if l.grant_id != grant_id || l.lineage != lineage {
                    return None;
                }
                if l.expires_ms < l.issued_ms || l.expires_ms - l.issued_ms > max_age {
                    return None;
                }
                let cert = self.certs.iter().find(|c| c.h_cert == l.issuer_cert)?;
                if cert.device_id == signer_device {
                    return None;
                }
                if !witnesses.contains(&cert.device_id) {
                    return None;
                }
                let kid = domains::key_id("ed25519", &cert.sig_pk);
                if self
                    .receipt_cutoffs
                    .get(&kid)
                    .is_some_and(|&thr| l.issuer_seq > thr)
                {
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
            // Accepted erase requests are tenant-side facts and carry
            // over; `manifested` and `receipt_cutoffs` are
            // control-derived and re-derive on the replay.
            erase_requests: std::mem::take(&mut self.erase_requests),
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
            // Accepted bytes parsed once at admission; a re-parse
            // failure is an internal-state invariant violation, not
            // a wire mechanism (P1 profile C.1 item 5).
            let op = parse_op(bytes).expect("accepted control bytes re-parse (D-138 replay)");
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

    /// Arm-general pin and signature validation — the pipeline's
    /// `arm` and `sig` stages (§7.2/D-91). D-76/D-99 stage order:
    /// parse, arm, sig, BODY, precedence/placement, state. A forged
    /// operation keeps its signature outcome and never freezes the
    /// plane, and a validly signed header over malformed or
    /// body-hash-mismatched bytes exerts NO precedence effect either
    /// (D-99 — v0.5.4 granted precedence before authenticating the
    /// body; the earlier comment here cited D-99 for the opposite of
    /// its resolution, the defect review finding R2 reproduced).
    fn ctrl_prevalidate(&self, op: &SignedOp) -> Result<(), Verdict> {
        Self::ctrl_header_pins(op)?;
        let pk = match op.header.proof {
            Proof::Admin { epoch, .. } => self.admin_key(epoch)?,
            Proof::Recovery {
                repoch,
                recovery_pk,
            } => {
                let Ok(pk) = <[u8; 32]>::try_from(recovery_pk) else {
                    return Err(Verdict::Rejected("proof-arm", "reject-permanent"));
                };
                let Some(commitment) = self.recovery_commitment else {
                    return Err(Verdict::Pending("ref-unresolved", "pending-dependency"));
                };
                if repoch > self.repoch + 1 || domains::h("drill", &pk) != commitment {
                    return Err(Verdict::Rejected("proof-arm", "reject-permanent"));
                }
                pk
            }
            Proof::Genesis { .. } | Proof::Dev { .. } => {
                return Err(Verdict::Rejected("proof-arm", "reject-permanent"))
            }
        };
        if !op.verify_ed25519(&pk) || op.header.signer_key_id != domains::key_id("ed25519", &pk) {
            return Err(Verdict::Rejected("sig-invalid", "reject-permanent"));
        }
        Ok(())
    }

    /// The body stage's arm-indexed CDDL/shape residue (§10.2
    /// `admit_ctrl`: body = hash → registry row → CDDL; D-99): the
    /// CLOSED KEY SETS (O3 — unknown fields in a registry body
    /// reject exactly as in headers; nested closed maps included,
    /// the ff23f1cd review's F2), the required members, coarse
    /// types, static caps, and byte-internal equalities of each
    /// DISPATCHED arm's registered body shape — every check the
    /// transition rejects `body-invariant` without reading state —
    /// evaluated BEFORE the replay consult and the placement gate
    /// (the criterion-12 F1 repair: a validly signed, hash-valid
    /// `c.grant` over `{bogus: 1}` — or over a valid grant PLUS a
    /// bogus sibling — classifies `(body-invariant,
    /// reject-permanent)`; it never derives `request-fork` or
    /// `ctrl-fork`). Taking NO `&self` makes byte-onlyness
    /// structural. What stays behind, deliberately: state-dependent
    /// invariants (the §10.2 state stage), wrap fields the
    /// transition only reaches after a state read (the enroll
    /// path's epoch gate), and every branch whose MECHANISM is an
    /// honest `Unimplemented` marker — its known CDDL key set is
    /// still enforced (the shape is ratified law even where the
    /// machine is deferred), but this stage returns `Ok` at the
    /// transition's abort point rather than inventing deeper law.
    /// `c.recovery_succession`'s base-binding and arm-agreement
    /// equalities stay in its §7.4 lane (prec-stage facts, not body
    /// shape). The module-level key-set tables mirror App A
    /// verbatim.
    fn ctrl_intrinsic_shape(op: &SignedOp) -> Result<(), Verdict> {
        let bad = || Verdict::Rejected("body-invariant", "reject-permanent");
        let body = &op.body;
        match op.header.operation_type {
            "c.enroll" => {
                let Some(cert) = body.get("cert") else {
                    return Err(bad());
                };
                if !keys_within(cert, CERT_REQ, CERT_OPT) {
                    return Err(bad());
                }
                if cert.get("renews").is_some() {
                    // Renewal union arm: the mechanism is an honest
                    // Unimplemented, but its CDDL key set is ratified
                    // law — unknown fields reject here.
                    if !keys_within(
                        body,
                        &["cert", "feed_closure", "history_cutoffs"],
                        &["wraps"],
                    ) {
                        return Err(bad());
                    }
                    return Ok(());
                }
                if !keys_within(body, &["cert", "grants", "lineage", "wraps"], &[]) {
                    return Err(bad());
                }
                let Some(device_id) = b16_field(cert, "device_id") else {
                    return Err(bad());
                };
                let sig_alg = cert.get("sig_alg").and_then(|n| n.as_text()).unwrap_or("");
                let sig_pk = cert.get("sig_pk").and_then(|n| n.as_bytes()).unwrap_or(&[]);
                let kem_pk = cert.get("kem_pk").and_then(|n| n.as_bytes()).unwrap_or(&[]);
                if sig_alg == "p256" && sig_pk == kem_pk {
                    // Intra-certificate role reuse (D-175).
                    return Err(bad());
                }
                let Some(lineage) = body.get("lineage") else {
                    return Err(bad());
                };
                if !keys_are_map(lineage, &["lineage", "device_id", "max_generations"]) {
                    return Err(bad());
                }
                let Some(lineage_id) = b16_field(lineage, "lineage") else {
                    return Err(bad());
                };
                if b16_field(lineage, "device_id") != Some(device_id) {
                    return Err(bad());
                }
                if let Some(grants) = body.get("grants").and_then(|g| g.as_array()) {
                    for gn in grants {
                        if b16_field(gn, "subject_device") != Some(device_id) {
                            return Err(bad());
                        }
                        Self::grant_intrinsic_shape(gn, Some((lineage_id, device_id)))?;
                    }
                }
                if let Some(wraps) = body.get("wraps").and_then(|w| w.as_array()) {
                    for wn in wraps {
                        // The wrap's key set is bytes; its epoch and
                        // recipient equalities sit behind the
                        // transition's state read + Unimplemented
                        // epoch gate.
                        if !keys_within(wn, KEKWRAP_KEYS, &[]) {
                            return Err(bad());
                        }
                        if b16_field(wn, "zone_id").is_none() {
                            return Err(bad());
                        }
                    }
                }
                Ok(())
            }
            "c.grant" => {
                if !keys_within(body, &["grant"], &[]) {
                    return Err(bad());
                }
                let Some(gn) = body.get("grant") else {
                    return Err(bad());
                };
                Self::grant_intrinsic_shape(gn, None)?;
                if b16_field(gn, "subject_device").is_none() {
                    return Err(bad());
                }
                Ok(())
            }
            "c.revoke_grant" => {
                if !keys_within(body, &["grant_id"], &["cutoff"]) {
                    return Err(bad());
                }
                if b16_field(body, "grant_id").is_none() {
                    return Err(bad());
                }
                // Whose zone/lineage the cutoff must name is the
                // revoked grant's (state); the frontierclose's own
                // shape is bytes.
                match body.get("cutoff") {
                    Some(cn) => Self::frontierclose_shape(cn),
                    None => Ok(()),
                }
            }
            "c.revoke_device" => {
                if !keys_within(
                    body,
                    &["mode", "revocation_id", "cutoffs", "rotation_refs"],
                    &["receipt_cutoffs"],
                ) {
                    return Err(bad());
                }
                let compromise = match body.get("mode").and_then(|m| m.as_text()) {
                    Some("exclude") => false,
                    Some("compromise") => true,
                    _ => return Err(bad()),
                };
                // Fully byte-only by construction (a static fn).
                Self::compound_receipt_cutoffs(body, compromise)?;
                if b16_field(body, "revocation_id").is_none() {
                    return Err(bad());
                }
                match body.get("rotation_refs").and_then(|r| r.as_array()) {
                    None => return Err(bad()),
                    Some(a) => {
                        if a.len() > 64 {
                            // E8.
                            return Err(bad());
                        }
                        for r in a {
                            if r.bytes_n::<32>().is_none() {
                                return Err(bad());
                            }
                        }
                    }
                }
                let Some(cs) = body.get("cutoffs").and_then(|c| c.as_array()) else {
                    return Err(bad());
                };
                for cn in cs {
                    Self::frontierclose_shape(cn)?;
                }
                Ok(())
            }
            "c.cutoff" => {
                if !keys_within(body, &["cutoffs"], &["closes", "requester"]) {
                    return Err(bad());
                }
                if body.get("requester").is_some() {
                    // Requester attestation: honest Unimplemented
                    // (the top-level key set above is still law).
                    return Ok(());
                }
                let Some(ratify) = body.get("cutoffs").and_then(|c| c.as_array()) else {
                    return Err(bad());
                };
                if !ratify.is_empty() {
                    // The ratify machine: honest Unimplemented.
                    return Ok(());
                }
                let closes = match body.get("closes").and_then(|c| c.as_array()) {
                    Some(a) if !a.is_empty() => a,
                    _ => return Err(bad()),
                };
                for cn in closes {
                    Self::frontierclose_shape(cn)?;
                }
                Ok(())
            }
            "c.cap_epoch_bump" => {
                if !keys_within(body, &["zone_id", "new_epoch"], &["cutoffs"]) {
                    return Err(bad());
                }
                let Some(zone) = b16_field(body, "zone_id") else {
                    return Err(bad());
                };
                if body.get("new_epoch").and_then(|n| n.as_uint()).is_none() {
                    return Err(bad());
                }
                Self::zone_cutoffs_shape(body, zone)
            }
            "c.zone_policy" => {
                if !keys_within(body, &["policy"], &["cutoffs"]) {
                    return Err(bad());
                }
                let Some(policy) = body.get("policy") else {
                    return Err(bad());
                };
                if !keys_within(policy, ZONEPOLICY_REQ, ZONEPOLICY_OPT) {
                    return Err(bad());
                }
                if policy.get("v").and_then(|n| n.as_uint()) != Some(1) {
                    return Err(bad());
                }
                let Some(zone) = b16_field(policy, "zone_id") else {
                    return Err(bad());
                };
                let strictness = policy.get("strictness").and_then(|n| n.as_text());
                if !matches!(strictness, Some("strict") | Some("lenient")) {
                    return Err(bad());
                }
                let fallback = policy.get("deadline_fallback").and_then(|n| n.as_text());
                let require_cert = policy
                    .get("require_cert_deadlines")
                    .and_then(|n| n.as_bool());
                match (fallback, require_cert) {
                    (Some("fail-closed"), Some(true)) | (Some("budgets"), Some(_)) => {}
                    _ => return Err(bad()),
                }
                match policy_witness_devices(Some(policy)) {
                    // A "connect" witness aborts the transition
                    // before its remaining checks — mirror the abort
                    // point, don't overtake it.
                    Err(_) => return Ok(()),
                    Ok(w) if w.len() > 64 => return Err(bad()),
                    Ok(_) => {}
                }
                if policy.get("connect_service_key").is_some() {
                    return Err(bad());
                }
                Self::zone_cutoffs_shape(body, zone)
            }
            "c.kek_rotate" => {
                if !keys_within(
                    body,
                    &["zone_id", "new_epoch", "wraps", "erase_manifest"],
                    &[],
                ) {
                    return Err(bad());
                }
                let zone = b16_field(body, "zone_id");
                if zone.is_none() {
                    return Err(bad());
                }
                let new_epoch = body.get("new_epoch").and_then(|n| n.as_uint());
                if new_epoch.is_none() {
                    return Err(bad());
                }
                match body.get("erase_manifest").and_then(|m| m.as_array()) {
                    None => return Err(bad()),
                    Some(entries) => {
                        if entries.len() > 128 {
                            // E8.
                            return Err(bad());
                        }
                        let mut prev_addr: Option<[u8; 32]> = None;
                        for e in entries {
                            if !keys_are_map(e, &["item_addr", "erase_op", "target_op"]) {
                                return Err(bad());
                            }
                            let (Some(addr), Some(_erase), Some(_target)) = (
                                e.get("item_addr").and_then(|v| v.bytes_n::<32>()),
                                e.get("erase_op").and_then(|v| v.bytes_n::<32>()),
                                e.get("target_op").and_then(|v| v.bytes_n::<32>()),
                            ) else {
                                return Err(bad());
                            };
                            if prev_addr.is_some_and(|p| p >= addr) {
                                return Err(bad());
                            }
                            prev_addr = Some(addr);
                        }
                    }
                }
                let wraps = match body.get("wraps").and_then(|w| w.as_array()) {
                    Some(a) if !a.is_empty() => a,
                    _ => return Err(bad()),
                };
                let mut recipients: Vec<[u8; 16]> = Vec::new();
                for wn in wraps {
                    // The wrap's static fields against the op's OWN
                    // zone/new_epoch (byte-internal; the plane and
                    // recipient-certificate checks are state).
                    if !keys_within(wn, KEKWRAP_KEYS, &[])
                        || wn.get("v").and_then(|n| n.as_uint()) != Some(1)
                        || wn.get("kem").and_then(|n| n.as_text()) != Some("hpke-p256-v1")
                        || wn.get("plane_id").and_then(|n| n.bytes_n::<32>()).is_none()
                        || b16_field(wn, "zone_id") != zone
                        || wn.get("epoch").and_then(|n| n.as_uint()) != new_epoch
                    {
                        return Err(bad());
                    }
                    let Some(r) = b16_field(wn, "recipient_device") else {
                        return Err(bad());
                    };
                    if recipients.contains(&r) {
                        // Duplicate set key (zone, epoch, device).
                        return Err(bad());
                    }
                    recipients.push(r);
                }
                Ok(())
            }
            "c.zone_create" => {
                if !keys_within(
                    body,
                    &["zone_id", "initial_epoch", "wraps", "zone_policy"],
                    &[],
                ) {
                    return Err(bad());
                }
                let Some(zone_id) = b16_field(body, "zone_id") else {
                    return Err(bad());
                };
                if body.get("initial_epoch").and_then(|n| n.as_uint()) != Some(1) {
                    return Err(bad());
                }
                let Some(policy) = body.get("zone_policy") else {
                    return Err(bad());
                };
                if !keys_within(policy, ZONEPOLICY_REQ, ZONEPOLICY_OPT)
                    || b16_field(policy, "zone_id") != Some(zone_id)
                {
                    return Err(bad());
                }
                let wraps = match body.get("wraps").and_then(|w| w.as_array()) {
                    Some(a) if !a.is_empty() => a,
                    _ => return Err(bad()),
                };
                for wn in wraps {
                    if !keys_within(wn, KEKWRAP_KEYS, &[])
                        || wn.get("v").and_then(|n| n.as_uint()) != Some(1)
                        || wn.get("kem").and_then(|n| n.as_text()) != Some("hpke-p256-v1")
                        || wn.get("plane_id").and_then(|n| n.bytes_n::<32>()).is_none()
                        || b16_field(wn, "zone_id") != Some(zone_id)
                        || wn.get("epoch").and_then(|n| n.as_uint()) != Some(1)
                        || b16_field(wn, "recipient_device").is_none()
                    {
                        return Err(bad());
                    }
                }
                Ok(())
            }
            "c.space_create" => {
                // cspacecreate = spacedef, the whole body.
                if !keys_within(body, SPACEDEF_KEYS, &[]) {
                    return Err(bad());
                }
                if b16_field(body, "space_id").is_none() || b16_field(body, "zone_id").is_none() {
                    return Err(bad());
                }
                Ok(())
            }
            "c.drill" => {
                if !keys_within(body, &["nonce"], &[]) {
                    return Err(bad());
                }
                if body.get("nonce").and_then(|n| n.bytes_n::<16>()).is_none() {
                    return Err(bad());
                }
                Ok(())
            }
            "c.recovery_succession" => {
                if !keys_within(
                    body,
                    &[
                        "base",
                        "epoch",
                        "repoch",
                        "new_admin",
                        "new_recovery_commitment",
                        "tenant_cutoffs",
                        "adopted_rotations",
                    ],
                    &["adopted_renewals", "retired_keys"],
                ) {
                    return Err(bad());
                }
                let Some(base) = body.get("base") else {
                    return Err(bad());
                };
                if !keys_are_map(base, &["seq", "op"])
                    || base.get("seq").and_then(|n| n.as_uint()).is_none()
                    || base.get("op").and_then(|n| n.bytes_n::<32>()).is_none()
                {
                    return Err(bad());
                }
                if body.get("repoch").and_then(|n| n.as_uint()).is_none()
                    || body.get("epoch").and_then(|n| n.as_uint()).is_none()
                {
                    return Err(bad());
                }
                let Some(new_admin) = body.get("new_admin") else {
                    return Err(bad());
                };
                if !keys_are_map(new_admin, &["alg", "pk"]) {
                    return Err(bad());
                }
                if new_admin.get("alg").and_then(|a| a.as_text()) != Some("ed25519") {
                    // Non-ed25519 successor: honest Unimplemented.
                    return Ok(());
                }
                if new_admin
                    .get("pk")
                    .and_then(|n| n.bytes_n::<32>())
                    .is_none()
                {
                    return Err(bad());
                }
                if body
                    .get("new_recovery_commitment")
                    .and_then(|n| n.bytes_n::<32>())
                    .is_none()
                {
                    return Err(bad());
                }
                if body.get("adopted_renewals").is_some() || body.get("retired_keys").is_some() {
                    // Renewal/freshness carriage: honest Unimplemented.
                    return Ok(());
                }
                match body.get("adopted_rotations").and_then(|a| a.as_array()) {
                    Some([]) => {}
                    // Adopted rotations: honest Unimplemented.
                    Some(_) => return Ok(()),
                    None => return Err(bad()),
                }
                let Some(cutoffs) = body.get("tenant_cutoffs").and_then(|c| c.as_array()) else {
                    return Err(bad());
                };
                for cn in cutoffs {
                    Self::frontierclose_shape(cn)?;
                }
                Ok(())
            }
            "c.genesis" => {
                // The transition is self-contained (arm/signature/
                // body one composition; C2/C5 skip it), but the
                // closed key sets are byte law like every arm's
                // (O3 — the ff23f1cd review's F2 class).
                if !keys_within(
                    body,
                    &[
                        "descriptor",
                        "cert",
                        "lineage",
                        "zone",
                        "home_space",
                        "audit_space",
                        "zone_policy",
                        "grant",
                        "audit_grant",
                    ],
                    &[],
                ) {
                    return Err(bad());
                }
                if let Some(zone) = body.get("zone") {
                    if !keys_are_map(zone, &["zone_id", "initial_epoch", "wraps"]) {
                        return Err(bad());
                    }
                }
                for g in ["grant", "audit_grant"] {
                    if let Some(gn) = body.get(g) {
                        Self::grant_intrinsic_shape(gn, None)?;
                    }
                }
                if let Some(cert) = body.get("cert") {
                    if !keys_within(cert, CERT_REQ, CERT_OPT) {
                        return Err(bad());
                    }
                }
                if let Some(policy) = body.get("zone_policy") {
                    if !keys_within(policy, ZONEPOLICY_REQ, ZONEPOLICY_OPT) {
                        return Err(bad());
                    }
                }
                for s in ["home_space", "audit_space"] {
                    if let Some(sd) = body.get(s) {
                        if !keys_within(sd, SPACEDEF_KEYS, &[]) {
                            return Err(bad());
                        }
                    }
                }
                Ok(())
            }
            // The registry rows whose mechanisms are honest
            // `Unimplemented` markers carry no intrinsic law here.
            _ => Ok(()),
        }
    }

    /// The grant object's byte-only gates (`grant_static_checks`
    /// minus its state reads): the closed key set (App A.1), the
    /// verb rules, and — for op-authoring grants — the finite zone
    /// plus the lineage/subject members. `enrolling` supplies the
    /// enroll/genesis shapes' byte-internal ownership equality;
    /// ownership against the lineage registry is state.
    fn grant_intrinsic_shape(
        gn: &Node,
        enrolling: Option<([u8; 16], [u8; 16])>,
    ) -> Result<(), Verdict> {
        let bad = || Verdict::Rejected("body-invariant", "reject-permanent");
        if !keys_within(gn, GRANT_REQ, GRANT_OPT) {
            return Err(bad());
        }
        if gn.get("plane_id").and_then(|n| n.bytes_n::<32>()).is_none() {
            return Err(bad());
        }
        let verbs: Vec<&str> = gn
            .get("ops")
            .and_then(|n| n.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_text()).collect())
            .unwrap_or_default();
        if verbs.is_empty() || verbs.iter().any(|v| !VERBS.contains(v)) || verbs.contains(&"admin")
        {
            return Err(bad());
        }
        if verbs.iter().any(|v| OP_AUTHORING.contains(v)) {
            let zone_finite = gn.get("zone").is_some_and(|z| z.bytes_n::<16>().is_some());
            let (l, s) = (b16_field(gn, "lineage"), b16_field(gn, "subject_device"));
            let (Some(l), Some(s)) = (l, s) else {
                return Err(bad());
            };
            if !zone_finite || enrolling.is_some_and(|e| e != (l, s)) {
                return Err(bad());
            }
        }
        Ok(())
    }

    /// A frontierclose's byte-only shape (App A.3): the closed key
    /// set, `zone_id` and `lineage` present, `heads` an array of
    /// exact `{lineage, gen, seq, op}` heads each naming the close's
    /// OWN lineage. Resolution against held chains (unheld pends,
    /// D-130 selection) is state.
    fn frontierclose_shape(cn: &Node) -> Result<(), Verdict> {
        let bad = || Verdict::Rejected("body-invariant", "reject-permanent");
        if !keys_are_map(cn, &["zone_id", "lineage", "heads"]) {
            return Err(bad());
        }
        let (Some(cl), Some(_cz)) = (b16_field(cn, "lineage"), b16_field(cn, "zone_id")) else {
            return Err(bad());
        };
        let Some(heads) = cn.get("heads").and_then(|h| h.as_array()) else {
            return Err(bad());
        };
        for hn in heads {
            if !keys_are_map(hn, &["lineage", "gen", "seq", "op"]) {
                return Err(bad());
            }
            let (Some(hl), Some(_gen), Some(_seq), Some(_hop)) = (
                b16_field(hn, "lineage"),
                hn.get("gen").and_then(|n| n.as_uint()),
                hn.get("seq").and_then(|n| n.as_uint()),
                hn.get("op").and_then(|n| n.bytes_n::<32>()),
            ) else {
                return Err(bad());
            };
            if hl != cl {
                return Err(bad());
            }
        }
        Ok(())
    }

    /// The `cutoffs` member's byte-only shape on the zone-scoped
    /// advances (`c.cap_epoch_bump` / `c.zone_policy`): every entry
    /// names THIS operation's zone with a well-formed frontierclose
    /// (live-lineage membership is state).
    fn zone_cutoffs_shape(body: &Node, zone: [u8; 16]) -> Result<(), Verdict> {
        let bad = || Verdict::Rejected("body-invariant", "reject-permanent");
        let Some(cs) = body.get("cutoffs") else {
            return Ok(());
        };
        let Some(cs) = cs.as_array() else {
            return Err(bad());
        };
        for cn in cs {
            if b16_field(cn, "zone_id") != Some(zone) {
                return Err(bad());
            }
            Self::frontierclose_shape(cn)?;
        }
        Ok(())
    }

    /// The pre-admission control gate (C2/C5, §7.4): a frozen plane
    /// admits no control op but the resolving recovery; a differing
    /// op at a HELD position is cut-branch material where a cut or
    /// recovery covers it, and fork evidence otherwise. Pins and the
    /// SIGNATURE stage precede every placement classification here
    /// (D-76 first-failing-stage; the D4 repair).
    fn ctrl_fork_gate(&mut self, op: &SignedOp) -> Result<Option<Verdict>, Unimplemented> {
        // The pipeline (classify) already ran arm/signature and body
        // validation — only sig- AND body-valid operations reach the
        // placement gate, so everything here is fork evidence (D-99).
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

    /// The D-74 release evaluation over an INDEPENDENT read-release
    /// event (review R4 — the pre-repair lane compared the reducer's
    /// surviving chunks against the vector's own expected list, so an
    /// incomplete partition "verified" as exact against itself). The
    /// released result ids and the read's one-Txn row set come from
    /// OUTSIDE the partition rows; the reducer derives:
    ///
    /// 1. completeness — the held rows' indexes are EXACTLY
    ///    `0..count−1` (a missing chunk refuses the release);
    /// 2. disjointness (re-derived, not assumed from admission);
    /// 3. exact union — ∪ row result_ids == the released ids, both
    ///    directions;
    /// 4. one-Txn membership — the read's row set equals the Txn's
    ///    row set exactly.
    ///
    /// `Err(reason)` = the release is REFUSED (fail-closed, D-52);
    /// the refusal's outcome vocabulary (`audit-unavailable` under
    /// durability failure) stays with the Gate-B edge lane — this is
    /// the exactness predicate made executable.
    pub(crate) fn audit_release_check(
        &self,
        read_id: [u8; 16],
        released: &[[u8; 32]],
        txn_rows: &[[u8; 32]],
    ) -> Result<(), &'static str> {
        let rows: Vec<&AuditRow> = self
            .audit_rows
            .iter()
            .filter(|r| r.read_id == read_id)
            .collect();
        let Some(count) = rows.first().map(|r| r.count) else {
            return Err("no audit rows for the read");
        };
        // Completeness: indexes exactly 0..count−1.
        let mut idxs: Vec<u64> = rows.iter().map(|r| r.index).collect();
        idxs.sort_unstable();
        if idxs != (0..count).collect::<Vec<u64>>() {
            return Err("partition incomplete: indexes are not exactly 0..count-1");
        }
        // Disjointness, re-derived.
        let mut seen: std::collections::BTreeSet<[u8; 32]> = std::collections::BTreeSet::new();
        for r in &rows {
            for id in &r.result_ids {
                if !seen.insert(*id) {
                    return Err("result sets overlap");
                }
            }
        }
        // Exact union, both directions.
        let released_set: std::collections::BTreeSet<[u8; 32]> = released.iter().copied().collect();
        if seen != released_set {
            return Err("row union differs from the released result set");
        }
        // One-Txn membership: the read's rows == the Txn's rows.
        let row_set: std::collections::BTreeSet<[u8; 32]> =
            rows.iter().map(|r| r.op_hash).collect();
        let txn_set: std::collections::BTreeSet<[u8; 32]> = txn_rows.iter().copied().collect();
        if row_set != txn_set {
            return Err("the read's rows did not ride the one declared Txn");
        }
        Ok(())
    }

    /// The final audit-partition chunk table — (index, count) pairs
    /// in index order (the audit-partition lane's assertion).
    pub(crate) fn audit_chunks(&self) -> Vec<(u64, u64)> {
        let mut chunks: Vec<(u64, u64)> =
            self.audit_rows.iter().map(|r| (r.index, r.count)).collect();
        chunks.sort_unstable();
        chunks
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
            if node.get("stmt").is_some() && node.get("sig").is_some() {
                self.hold_statement(&node)?;
                continue;
            }
            return Err(Unimplemented(format!("aux {name}: unrecognized shape")));
        }
        Ok(())
    }

    /// Hold one signed statement — `{stmt, sig}` — as §4.7 evidence:
    /// accept receipts and leases enter their registries; other
    /// statement kinds are held inert. Shared by aux installation
    /// and DELIVERED evidence (the D-202 lifecycle lane).
    pub(crate) fn hold_statement(&mut self, node: &Node) -> Result<(), Unimplemented> {
        {
            let name = "statement";
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
                    let (Some(subject), Some(seen_ms), Some(issuer_seq)) = (
                        stmt.get("subject").and_then(|n| n.bytes_n::<32>()),
                        stmt.get("seen_ms").and_then(|n| n.as_uint()),
                        stmt.get("issuer_seq").and_then(|n| n.as_uint()),
                    ) else {
                        return Err(Unimplemented(format!("aux {name}: accept shape")));
                    };
                    self.receipts.push(AuxReceipt {
                        issuer_cert,
                        plane,
                        zone,
                        subject,
                        seen_ms,
                        issuer_seq,
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
                let Some(issuer_seq) = stmt.get("issuer_seq").and_then(|n| n.as_uint()) else {
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
                    issuer_seq,
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
    /// non-human unattested writer (autonomous daemon/browser) is
    /// `None` = NO CLASS by the owner's D2 ruling (2026-07-14,
    /// alternative (c), decisions-pending.md): its judgments may be
    /// recorded where authoring verbs admit them but count toward
    /// NO status rule — an agent wanting status influence gets
    /// attested (the session path).
    fn actor_class(op: &SignedOp, grant: &HeldGrant) -> Option<&'static str> {
        let h = &op.header;
        let human = h.actor_kind == "human" && h.attested_by.is_none();
        if human {
            let full = grant
                .verbs
                .iter()
                .any(|v| matches!(v.as_str(), "judge.full" | "pin.full" | "curate.instruction"));
            Some(if full { "owner" } else { "safe-human" })
        } else if h.actor_kind == "service" {
            Some("service")
        } else if h.attested_by.is_some() {
            Some("session")
        } else {
            None
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
        // Retain the time basis for deadline/lease-bearing ops (T4).
        let time = {
            let held_cert = match h.proof {
                Proof::Dev { cert, .. } => self.certs.iter().find(|c| c.h_cert == cert),
                _ => None,
            };
            let mut deadlines: Vec<u64> = Vec::new();
            deadlines.extend(grant.expiry_deadline_ms);
            deadlines.extend(held_cert.and_then(|c| c.expiry_deadline_ms));
            match held_cert {
                Some(cert) if !deadlines.is_empty() || grant.online_lease => Some(TimeFacts {
                    signer_device: cert.device_id,
                    deadlines,
                    lease: grant
                        .online_lease
                        .then(|| (grant.grant_id, grant.max_age_ms.unwrap_or(0))),
                }),
                _ => None,
            }
        };
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
            cap_epoch: h.capability_epoch,
            op_len: op.raw.len() as u64,
            claim,
            release,
            import,
            judge,
            time,
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
        self.erase_requests.insert(op.op_hash(), resolved.clone());
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
    /// Is `h` an ACCEPTED operation on the current control chain
    /// (the D-138 log — a cut op is not)?
    pub(crate) fn ctrl_accepted(&self, h: &[u8; 32]) -> bool {
        self.ctrl_log
            .iter()
            .any(|(_, bytes)| parse_op(bytes).is_ok_and(|op| op.op_hash() == *h))
    }

    /// The §7.4 derived classification of a control op the chain
    /// re-classified (a C2 freeze or a C3′/recovery cut) — `None`
    /// when the chain never overlaid it.
    pub(crate) fn ctrl_overlaid(&self, h: &[u8; 32]) -> Option<Verdict> {
        self.ctrl_overlay.get(h).copied()
    }

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
    /// `m.audit` (§11.1, D-74/D-83): a SERVICE actor attested by the
    /// writing device's own certificate appends partition rows on
    /// the audit space — one read = one `read_id`, one principal,
    /// one canonical scope, one zone; chunk indexes exactly
    /// `0..count−1` with disjoint result sets.
    fn admit_audit(&mut self, op: &SignedOp) -> Result<Result<(), Verdict>, Unimplemented> {
        let grant = match self.tenant_preamble(op, "audit.write")? {
            Ok(g) => g,
            Err(v) => return ok(Err(v)),
        };
        let h = &op.header;
        let bad = || Verdict::Rejected("body-invariant", "reject-permanent");
        // Row-level actor rules: service kind, attested by the
        // writing device's OWN certificate; space class audit.
        let Proof::Dev { cert, .. } = h.proof else {
            return ok(Err(Verdict::Rejected("proof-arm", "reject-permanent")));
        };
        if h.actor_kind != "service" || h.attested_by != Some(cert) {
            return ok(Err(bad()));
        }
        if !self
            .spaces
            .iter()
            .any(|s| s.space_id == h.space_id && s.space_class == "audit")
        {
            return ok(Err(bad()));
        }
        let body = &op.body;
        if !keys_are_map(
            body,
            &[
                "principal",
                "read_id",
                "chunk",
                "scope",
                "result_ids",
                "at_ms",
            ],
        ) {
            return ok(Err(bad()));
        }
        // Typed principal: one of the five closed auditprin shapes.
        let Some(principal) = body.get("principal") else {
            return ok(Err(bad()));
        };
        let legal_principal = keys_are_map(principal, &["shape", "device"])
            || keys_are_map(principal, &["shape", "device", "session"])
            || keys_are_map(principal, &["shape", "token_hash"])
            || keys_are_map(principal, &["shape", "kind", "peer"])
            || keys_are_map(principal, &["shape", "kind", "peer", "token_hash"])
            || keys_are_map(principal, &["shape", "kind", "session", "token_hash"]);
        if !legal_principal {
            return ok(Err(bad()));
        }
        let (Some(read_id), Some(chunk), Some(scope), Some(at)) = (
            body.get("read_id").and_then(|n| n.bytes_n::<16>()),
            body.get("chunk"),
            body.get("scope"),
            body.get("at_ms").and_then(|n| n.as_uint()),
        ) else {
            return ok(Err(bad()));
        };
        let _ = at; // diagnostic local time — never authority (D-64)
        let (Some(index), Some(count)) = (
            chunk.get("index").and_then(|n| n.as_uint()),
            chunk.get("count").and_then(|n| n.as_uint()),
        ) else {
            return ok(Err(bad()));
        };
        // Indexes exactly 0..count−1.
        if count == 0 || index >= count {
            return ok(Err(bad()));
        }
        // One read = one zone; spaces a bounded set (≤ 64).
        if scope.get("zone").and_then(|n| n.bytes_n::<16>()).is_none() {
            return ok(Err(bad()));
        }
        let Some(spaces) = scope.get("spaces").and_then(|n| n.as_array()) else {
            return ok(Err(bad()));
        };
        if spaces.is_empty() || spaces.len() > 64 {
            return ok(Err(bad()));
        }
        let mut result_ids = Vec::new();
        for r in body
            .get("result_ids")
            .and_then(|n| n.as_array())
            .unwrap_or(&[])
        {
            let Some(id) = r.bytes_n::<32>() else {
                return ok(Err(bad()));
            };
            result_ids.push(id);
        }
        // The partition invariants against the read's HELD rows:
        // shared principal/scope/count, fresh index, disjoint sets.
        let principal_raw = principal.raw.to_vec();
        let scope_raw = scope.raw.to_vec();
        for held in self.audit_rows.iter().filter(|r| r.read_id == read_id) {
            if held.principal_raw != principal_raw
                || held.scope_raw != scope_raw
                || held.count != count
                || held.index == index
                || held.result_ids.iter().any(|r| result_ids.contains(r))
            {
                return ok(Err(bad()));
            }
        }

        // Accept.
        self.record_tenant(op, &grant, None, None, None, None);
        self.audit_rows.push(AuditRow {
            op_hash: op.op_hash(),
            read_id,
            principal_raw,
            scope_raw,
            index,
            count,
            result_ids,
        });
        ok(Ok(()))
    }

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

        // Verb selection (§11.1 rows). A bare non-human unattested
        // writer has NO class (the owner's D2 ruling, alternative
        // (c), 2026-07-14): no judge-verb row ever admits it —
        // holding a judge verb fails the row invariant like any
        // other class/evidence miss — while the author-relation
        // authoring-verb paths below stay open (recorded; inert in
        // the status fold).
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
                && j.actor_class.is_some_and(|c| r.actor_classes.contains(&c))
                && relation_holds(r.relation)
        })
    }

    /// The §11.2 status fold. `as_of` is carried for the temporal
    /// terms (none of the corpus claims carry validity windows yet —
    /// the parameter is threaded, unused). Cycle detection per rule 2
    /// (a supersession cycle derives `disputed`). A standing
    /// bare-writer judgment participates like any other admitted
    /// judgment EXCEPT that its `None` class matches no policy rule
    /// (the D2 ruling: recorded, never counting).
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
            "m.audit" => self.admit_audit(op),
            "m.judge" => self.admit_judge(op),
            "m.export.release" => self.admit_release(op),
            "m.import.claim" => self.admit_import(op),
            "m.erase_request" => self.admit_erase_request(op),
            // Registry-known types whose mechanisms are Unimplemented
            // or fail-closed stay honest markers; a type OUTSIDE the
            // closed §7.1/§11.1 registry is `op-unknown`.
            other if REGISTRY_OP_TYPES.contains(&other) => {
                Err(Unimplemented(format!("op_type {other}")))
            }
            _ => ok(Err(Verdict::Rejected("op-unknown", "reject-permanent"))),
        }
    }
}

/// The closed §7.1/§11.1 operation-type registry (dispatch-known
/// types plus registry rows whose mechanisms are not yet dispatched).
const REGISTRY_OP_TYPES: &[&str] = &[
    "c.abandon_writer",
    "c.admin_succession",
    "c.cap_epoch_bump",
    "c.checkpoint",
    "c.cutoff",
    "c.drill",
    "c.enroll",
    "c.enroll_renew",
    "c.genesis",
    "c.grant",
    "c.kek_rotate",
    "c.lineage_reauth",
    "c.recovery_succession",
    "c.revoke_device",
    "c.revoke_grant",
    "c.revoke_zones",
    "c.service_key",
    "c.space_create",
    "c.space_policy_set",
    "c.space_retire",
    "c.wrap_add",
    "c.zone_create",
    "c.zone_policy",
    "m.audit",
    "m.claim",
    "m.erase_request",
    "m.export.release",
    "m.import.claim",
    "m.judge",
    "m.pin",
    "m.unpin",
];

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
///
/// **Order convergence is structural here (review finding R1):** the
/// state after delivery position `i` is `canonical_fold(the SET of
/// items delivered through i)` — recomputed from scratch against a
/// content-derived canonical processing order, so the final state is
/// a pure function of the delivered set and CANNOT depend on arrival
/// order. The earlier engine resolved pending operations in arrival
/// order with a mutating classifier, and eight committed vectors
/// reached different durable states under legal unlisted orders.
///
/// Byte-identical items fold ONCE: the lexicographically first name
/// in a byte-group carries the operation's verdict; every other name
/// reports the delivery-edge `duplicate` — a canonical assignment,
/// so the label too is arrival-independent (the review's comparator
/// normalization).
pub fn run_delivery_full(
    items: &BTreeMap<String, Vec<u8>>,
    aux: &BTreeMap<String, Vec<u8>>,
    order: &[String],
) -> Result<(Run, State), Unimplemented> {
    // name → its byte-group's canonical (lexicographically first) name.
    let mut group_of: BTreeMap<&String, &String> = BTreeMap::new();
    {
        let mut by_bytes: BTreeMap<&[u8], &String> = BTreeMap::new();
        let mut names: Vec<&String> = items.keys().collect();
        names.sort();
        for name in names {
            let canon = by_bytes.entry(items[name].as_slice()).or_insert(name);
            group_of.insert(name, canon);
        }
    }

    let mut snapshots = Vec::new();
    let mut last: Option<(BTreeMap<String, Verdict>, State)> = None;
    for i in 0..order.len() {
        let folded = canonical_fold(items, aux, &order[..=i], &group_of)?;
        snapshots.push(folded.0.clone());
        last = Some(folded);
    }
    let (final_verdicts, state) = match last {
        Some(v) => v,
        None => {
            let mut state = State::default();
            state.install_aux(aux)?;
            (BTreeMap::new(), state)
        }
    };
    Ok((
        Run {
            final_verdicts,
            snapshots,
        },
        state,
    ))
}

/// The content-derived canonical processing key: control operations
/// by chain position, tenant operations by their (zone, lineage,
/// gen, seq) coordinate, hash-tied last — NEVER the fixture name or
/// the arrival position.
pub(crate) type CanonicalKey = (u8, [u8; 16], [u8; 16], u64, u64, [u8; 32]);

pub(crate) fn canonical_key(bytes: &[u8]) -> CanonicalKey {
    match parse_op(bytes) {
        Ok(op) => {
            let h = &op.header;
            let class = u8::from(!h.operation_type.starts_with("c."));
            (
                class,
                h.zone_id,
                h.writer_lineage,
                h.writer_gen,
                h.writer_sequence,
                op.op_hash(),
            )
        }
        Err(_) => (2, [0; 16], [0; 16], 0, 0, domains::h("op", bytes)),
    }
}

/// One canonical fold of a delivered subset: classify every unique
/// operation in canonical order, retrying the pending set to a
/// verdict fixpoint, then overlay the derived lanes (§10.5). The
/// round cap makes non-stabilization an honest error instead of a
/// hang (verdicts can only flip while state grows; growth is bounded
/// by the subset).
fn canonical_fold(
    items: &BTreeMap<String, Vec<u8>>,
    aux: &BTreeMap<String, Vec<u8>>,
    subset: &[String],
    group_of: &BTreeMap<&String, &String>,
) -> Result<(BTreeMap<String, Verdict>, State), Unimplemented> {
    let mut state = State::default();
    state.install_aux(aux)?;

    // Unique canonical names in the subset, content-ordered.
    let mut uniq: Vec<&String> = subset
        .iter()
        .map(|n| *group_of.get(n).expect("delivered names exist"))
        .collect();
    uniq.sort();
    uniq.dedup();
    uniq.sort_by_key(|n| canonical_key(&items[*n]));

    let mut verdicts: BTreeMap<String, Verdict> = BTreeMap::new();
    let rounds_cap = uniq.len() * 2 + 4;
    let mut stabilized = false;
    for _ in 0..rounds_cap {
        let mut changed = false;
        for name in &uniq {
            match verdicts.get(*name) {
                Some(Verdict::Admitted) | Some(Verdict::Rejected(..)) => continue,
                _ => {}
            }
            let v = classify(&mut state, &items[*name])?;
            if verdicts.get(*name) != Some(&v) {
                changed = true;
                verdicts.insert((*name).clone(), v);
            }
        }
        if !changed {
            stabilized = true;
            break;
        }
    }
    if !stabilized {
        return Err(Unimplemented(
            "canonical fold did not stabilize within the round cap".into(),
        ));
    }

    // The derived lanes (§10.5): a held tenant op's fold verdict is a
    // projection of current state; control ops re-classified by a
    // freeze or a cut overlay from §7.4.
    let derived = state.derived_tenant_verdicts()?;
    for name in &uniq {
        if let Ok(op) = parse_op(&items[*name]) {
            let h = op.op_hash();
            if let Some(v) = derived.get(&h) {
                verdicts.insert((*name).clone(), *v);
            }
            if let Some(v) = state.ctrl_overlay.get(&h) {
                verdicts.insert((*name).clone(), *v);
            }
        }
    }

    // Delivery-edge duplicates: canonical, never arrival-relative.
    let mut out = BTreeMap::new();
    for name in subset {
        let canon = *group_of.get(name).expect("delivered names exist");
        if *name == *canon {
            out.insert(name.clone(), verdicts[canon]);
        } else {
            out.insert(
                name.clone(),
                Verdict::Rejected("duplicate", "duplicate-idempotent"),
            );
        }
    }
    Ok((out, state))
}

/// The D-202 evidence-lifecycle runner: POSITIONAL evolution —
/// arrival order is the lane's semantic INPUT (the owner's ruling
/// sanctions per-replica divergence on the original operation;
/// convergence rides the re-proposed one), so unlike the canonical
/// fold this loop evolves one state across delivery positions.
/// After every arrival the non-final set re-evaluates — INCLUDING
/// revivable (quarantine-reproposal) rejections, so stickiness is
/// demonstrated, never assumed: without the `stale_issued` registry
/// a stale op would revive the moment timely evidence lands, and
/// the lifecycle vector would fail.
pub fn run_lifecycle(
    items: &BTreeMap<String, Vec<u8>>,
    aux: &BTreeMap<String, Vec<u8>>,
    order: &[String],
) -> Result<Run, Unimplemented> {
    let mut state = State::default();
    state.install_aux(aux)?;
    let mut verdicts: BTreeMap<String, Verdict> = BTreeMap::new();
    let mut hashes: BTreeMap<String, [u8; 32]> = BTreeMap::new();
    let mut snapshots = Vec::new();
    let mut arrived: Vec<String> = Vec::new();
    for name in order {
        if let Ok(op) = parse_op(&items[name]) {
            hashes.insert(name.clone(), op.op_hash());
        }
        arrived.push(name.clone());
        let v = classify(&mut state, &items[name])?;
        verdicts.insert(name.clone(), v);
        let rounds_cap = arrived.len() * 2 + 4;
        let mut stabilized = false;
        for _ in 0..rounds_cap {
            let mut changed = false;
            for n in &arrived {
                match verdicts.get(n) {
                    Some(Verdict::Admitted) => continue,
                    // Revivable rejections re-evaluate (the sticky
                    // registry is what keeps an issued stale stale);
                    // permanent ones are final.
                    Some(Verdict::Rejected(_, d)) if *d != "quarantine-reproposal" => continue,
                    _ => {}
                }
                let v = classify(&mut state, &items[n])?;
                if verdicts.get(n) != Some(&v) {
                    changed = true;
                    verdicts.insert(n.clone(), v);
                }
            }
            if !changed {
                stabilized = true;
                break;
            }
        }
        if !stabilized {
            return Err(Unimplemented(
                "lifecycle evaluation did not stabilize within the round cap".into(),
            ));
        }
        let derived = state.derived_tenant_verdicts()?;
        for (n, h) in &hashes {
            if let Some(v) = derived.get(h) {
                verdicts.insert(n.clone(), *v);
            }
            if let Some(v) = state.ctrl_overlay.get(h) {
                verdicts.insert(n.clone(), *v);
            }
        }
        snapshots.push(verdicts.clone());
    }
    Ok(Run {
        final_verdicts: verdicts,
        snapshots,
    })
}

/// Classify one operation against the state — the §7.2/§10.2
/// arm-indexed pipeline: parse → arm → sig → body → replay →
/// precedence/placement → state, with the D-112 transition-last
/// discipline made STRUCTURAL: the admission stages run against a
/// staged clone that commits only when the operation is accepted or
/// deliberately pends (a reservation); a REJECTED operation exerts
/// no state effect. The two deliberate real-state effects before the
/// clone are evidence semantics, not transitions: a C2 freeze and
/// D-130 fork-evidence registration both persist because valid
/// conflicting SIGNATURES are facts about the plane, not about
/// either operation's acceptance.
pub(crate) fn classify(state: &mut State, bytes: &[u8]) -> Result<Verdict, Unimplemented> {
    let op = match parse_op(bytes) {
        Ok(op) => op,
        Err(err) => {
            // Delivered EVIDENCE (§4.7): a signed statement arriving
            // on the wire — `{stmt, sig}` exactly — enters the held
            // context like its aux twin (the D-202 lifecycle lane
            // delivers receipts as events; the review's R7 asked for
            // exactly this executable form).
            if let Ok(node) = crate::cbor::decode(bytes) {
                if keys_are_map(&node, &["stmt", "sig"]) {
                    state.hold_statement(&node)?;
                    return Ok(Verdict::Admitted);
                }
            }
            match err {
                crate::envelope::OpError::Parse(e) => {
                    use crate::cbor::DecodeError as D;
                    let outcome = match e {
                        D::Depth => "depth",
                        D::NonCanonical | D::UintRange => "non-canonical",
                        D::Malformed | D::TrailingBytes => "malformed",
                    };
                    return Ok(Verdict::Rejected(outcome, "reject-permanent"));
                }
                crate::envelope::OpError::Version => {
                    return Ok(Verdict::Rejected("unknown-version", "reject-permanent"));
                }
                crate::envelope::OpError::Shape(_) => {
                    return Ok(Verdict::Rejected("malformed", "reject-permanent"));
                }
            }
        }
    };

    // D-202 stickiness: an issued `lease-stale` is TERMINAL on this
    // replica — the memoized verdict answers every re-evaluation
    // before any pipeline stage runs (were the pipeline re-entered,
    // the re-proposal now holding the freed position would read as
    // fork evidence; the issue decided this op for good, and the
    // re-proposed op is the convergence carrier WITHIN this
    // evidence-arrival structure — D-204: a timely-first replica
    // admits the original instead, and the pair freezes there).
    if state.stale_issued.contains(&op.op_hash()) {
        return Ok(Verdict::Rejected("lease-stale", "quarantine-reproposal"));
    }

    if op.header.operation_type.starts_with("c.") {
        // arm + sig (genesis is self-contained — admit_genesis
        // verifies under the descriptor's own root key).
        if op.header.operation_type != "c.genesis" {
            if let Err(v) = state.ctrl_prevalidate(&op) {
                return Ok(v);
            }
        }
        // body: the hash binding precedes EVERY precedence effect
        // (D-99 — a validly signed header over mismatched bytes
        // never suppresses C2); the arm's registry row precedes
        // placement too.
        if !op.body_hash_ok() {
            return Ok(Verdict::Rejected("body-hash", "reject-permanent"));
        }
        if !REGISTRY_OP_TYPES.contains(&op.header.operation_type) {
            return Ok(Verdict::Rejected("op-unknown", "reject-permanent"));
        }
        // The registry row is keyed by ALL THREE coordinates —
        // (tenant, operation_type, operation_version), the ff23f1cd
        // review's F3: an unsupported semantic version rejects
        // `unknown-version` at the row consult, before the arm's
        // CDDL, the replay consult, and placement (every v1 registry
        // row is operation_version 1; the header's OWN `v` is the
        // protocol version, rejected at parse).
        if op.header.operation_version != 1 {
            return Ok(Verdict::Rejected("unknown-version", "reject-permanent"));
        }
        // ...and the arm's intrinsic CDDL shape completes the body
        // stage (hash → registry row → CDDL, §10.2/D-99) before ANY
        // precedence consult — the criterion-12 F1 repair: a validly
        // signed, hash-valid `c.grant` over `{bogus: 1}` classifies
        // `(body-invariant, reject-permanent)`, never `request-fork`
        // or `ctrl-fork`. State-dependent invariants stay in the
        // transition.
        if let Err(v) = State::ctrl_intrinsic_shape(&op) {
            return Ok(v);
        }
        // replay: consulted post-validity; consumed at acceptance.
        if let Some(v) = state.request_check(&op) {
            return Ok(v);
        }
        // placement (C2/C5) — recovery carries its own placement
        // rules (the §7.4 precedence exception). The freeze is an
        // evidence effect and commits on the real state.
        if op.header.operation_type != "c.genesis"
            && op.header.operation_type != "c.recovery_succession"
        {
            if let Some(v) = state.ctrl_fork_gate(&op)? {
                return Ok(v);
            }
        }
    }

    // state: the transactional admission (tenant validity stages and
    // the replay consult live inside tenant_preamble, in pipeline
    // order).
    let replay_key = (
        op.header.zone_id,
        op.header.writer_lineage,
        op.header.request_id,
    );
    let mut staged = state.clone();
    let r = staged.admit(&op)?;
    Ok(match r {
        Ok(()) => {
            staged.request_seen.insert(replay_key, op.op_hash());
            if op.header.operation_type.starts_with("c.") {
                staged
                    .ctrl_log
                    .push((op.header.writer_sequence, bytes.to_vec()));
            }
            *state = staged;
            Verdict::Admitted
        }
        Err(v @ Verdict::Pending(..)) => {
            // Deliberate pending effects commit (a compound's
            // reservation holds its chain position, D-195).
            *state = staged;
            v
        }
        Err(v) => {
            // Rejected: the staged state is discarded — no failed
            // operation exerts precedence (D-112). Two evaluation-
            // history facts persist on the real state: fork evidence
            // (the preamble's chain stage fired only after the
            // validity stages passed) and a D-202 lease-stale ISSUE
            // (terminal where issued).
            if v == Verdict::Rejected("fork", "freeze-writer")
                && !op.header.operation_type.starts_with("c.")
            {
                state.register_tenant_fork(&op);
            }
            if v == Verdict::Rejected("lease-stale", "quarantine-reproposal") {
                state.stale_issued.insert(op.op_hash());
            }
            v
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PRE-REPAIR engine, kept test-only as the review's
    /// acceptance-criterion-3 control: per-arrival classification
    /// with the pending set retried in ARRIVAL order until no new
    /// ADMISSION (reservation progress invisible — exactly the
    /// mechanism behind the r2 pending/rejected flip the review
    /// reproduced). The discrimination test proves the convergence
    /// standard fails under a deliberate restoration of this loop.
    fn arrival_ordered_delivery(
        items: &BTreeMap<String, Vec<u8>>,
        order: &[&str],
    ) -> BTreeMap<String, Verdict> {
        let mut state = State::default();
        let mut verdicts: BTreeMap<String, Verdict> = BTreeMap::new();
        let mut pending: Vec<String> = Vec::new();
        let mut hashes: BTreeMap<String, [u8; 32]> = BTreeMap::new();
        for name in order {
            let bytes = &items[*name];
            if let Ok(op) = parse_op(bytes) {
                hashes.insert(name.to_string(), op.op_hash());
            }
            let v = classify(&mut state, bytes).unwrap();
            verdicts.insert(name.to_string(), v);
            if matches!(v, Verdict::Pending(..)) {
                pending.push(name.to_string());
            }
            loop {
                let mut progressed = false;
                let mut still = Vec::new();
                for pname in pending.drain(..) {
                    let v = classify(&mut state, &items[&pname]).unwrap();
                    verdicts.insert(pname.clone(), v);
                    match v {
                        Verdict::Pending(..) => still.push(pname),
                        Verdict::Admitted => progressed = true,
                        Verdict::Rejected(..) => {}
                    }
                }
                pending = still;
                if !progressed {
                    break;
                }
            }
            let derived = state.derived_tenant_verdicts().unwrap();
            for (n, h) in &hashes {
                if verdicts.get(n) == Some(&Verdict::Rejected("duplicate", "duplicate-idempotent"))
                {
                    continue;
                }
                if let Some(v) = derived.get(h) {
                    verdicts.insert(n.clone(), *v);
                }
                if let Some(v) = state.ctrl_overlay.get(h) {
                    verdicts.insert(n.clone(), *v);
                }
            }
        }
        verdicts
    }

    /// Acceptance criterion 3: the convergence standard DISCRIMINATES
    /// — the restored arrival-ordered loop diverges on the review's
    /// R1 order while the canonical engine converges on it.
    #[test]
    fn convergence_standard_fails_under_arrival_order_restoration() {
        let (items, _) = load("f07-second-live-compound-rejects.json");
        let review_order = ["r2", "r1", "c1", "c2"];
        let restored = arrival_ordered_delivery(&items, &review_order);

        let sorted: Vec<String> = items.keys().cloned().collect();
        let (fresh, _) = run_delivery_full(&items, &BTreeMap::new(), &sorted).unwrap();
        assert_ne!(
            restored, fresh.final_verdicts,
            "the restored engine must diverge on the review order — otherwise the \
             metamorphic suite tests nothing"
        );

        let order: Vec<String> = review_order.iter().map(|s| s.to_string()).collect();
        let (run, _) = run_delivery_full(&items, &BTreeMap::new(), &order).unwrap();
        assert_eq!(run.final_verdicts, fresh.final_verdicts);
    }

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
