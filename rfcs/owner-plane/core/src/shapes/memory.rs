//! Appendix A.5 — memory-tenant bodies (§11, operation_version 1),
//! A.6 — the policy object (§11.3), and the §11.8 bundle Merkle
//! (D-147/D-156/D-162): self-describing `bundleleaf` leaves under
//! `H_brec`, `H_bnode(left ‖ right)` parents, odd nodes promoting
//! unchanged, bottom-up exact-consumption proofs (≤ 7 under the
//! 128-source cap).

use super::identity::Endpoint;
use super::{
    bytes, closed_vocab, key_bytes, sorted_set_by_key, sorted_set_default, text, u, Bytes16,
    Bytes32, Class, Kind, Polref, Spaceclass, ToValue,
};
use crate::cbor::{self, Value};
use crate::domains::{h_tag, Tag};

/// `bundlerec = { v: 1, op: bytes32, kind: kind, statement: text,
///   class_floor: class }` — deterministic redaction (§11.8);
/// class_floor = the source's effective classification at the
/// release stamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundlerec {
    pub op: Bytes32,
    pub kind: Kind,
    pub statement: String,
    pub class_floor: Class,
}

impl ToValue for Bundlerec {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("v", u(1)),
            ("op", bytes(&self.op)),
            ("kind", self.kind.to_value()),
            ("statement", text(&self.statement)),
            ("class_floor", self.class_floor.to_value()),
        ])
    }
}

/// `bundleleaf = { v: 1, export_id: bytes16, rec_index: uint,
///   rec: bundlerec }` — the NAMED leaf preimage (D-162);
/// `leaf = H_brec(canonical bundleleaf)`; rec_index = the record's
/// 0-based rank in the release's signed, sorted sources (D-156).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundleleaf {
    pub export_id: Bytes16,
    pub rec_index: u64,
    pub rec: Bundlerec,
}

impl ToValue for Bundleleaf {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("v", u(1)),
            ("export_id", bytes(&self.export_id)),
            ("rec_index", u(self.rec_index)),
            ("rec", self.rec.to_value()),
        ])
    }
}

impl Bundleleaf {
    /// `leaf = H_brec(canonical bundleleaf)`.
    pub fn leaf_hash(&self) -> Bytes32 {
        h_tag(
            Tag::Brec,
            &cbor::encode(&self.to_value()).expect("bundleleaf encodes"),
        )
    }
}

/// `bundle = { v: 1, export_id: bytes16, recs: [+ bundlerec] }` —
/// recs: set keyed op, EXACTLY the release's sources; bundles are
/// re-derived, never persisted or framed (D-75); NO release_op in
/// the digest preimage (D-127).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bundle {
    pub export_id: Bytes16,
    pub recs: Vec<Bundlerec>,
}

impl ToValue for Bundle {
    fn to_value(&self) -> Value {
        let recs = sorted_set_by_key(
            self.recs
                .iter()
                .map(|r| (key_bytes(&[bytes(&r.op)]), r.to_value()))
                .collect(),
            "bundle.recs (key op)",
        );
        cbor::map(vec![
            ("v", u(1)),
            ("export_id", bytes(&self.export_id)),
            ("recs", Value::Array(recs)),
        ])
    }
}

impl Bundle {
    /// The record-rank-ordered leaves: recs sorted by `op` (the
    /// release's signed, sorted sources order — D-156), each wrapped
    /// as a self-describing `bundleleaf` and hashed under `H_brec`.
    pub fn leaves(&self) -> Vec<Bytes32> {
        let mut sorted: Vec<&Bundlerec> = self.recs.iter().collect();
        sorted.sort_by_key(|r| r.op);
        sorted
            .iter()
            .enumerate()
            .map(|(i, rec)| {
                Bundleleaf {
                    export_id: self.export_id,
                    rec_index: i as u64,
                    rec: (*rec).clone(),
                }
                .leaf_hash()
            })
            .collect()
    }

    /// `content_digest` = the Merkle root over the leaves (§11.8).
    pub fn content_digest(&self) -> Bytes32 {
        merkle_root(&self.leaves())
    }
}

/// `parents = H_bnode(left || right); an odd node promotes unchanged`
/// (§11.8). A single leaf is its own root.
pub fn merkle_root(leaves: &[Bytes32]) -> Bytes32 {
    assert!(!leaves.is_empty(), "a bundle has at least one record");
    let mut level: Vec<Bytes32> = leaves.to_vec();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            if pair.len() == 2 {
                let mut cat = [0u8; 64];
                cat[..32].copy_from_slice(&pair[0]);
                cat[32..].copy_from_slice(&pair[1]);
                next.push(h_tag(Tag::Bnode, &cat));
            } else {
                next.push(pair[0]); // odd node promotes unchanged
            }
        }
        level = next;
    }
    level[0]
}

/// The bottom-up sibling path for `index` (a promoted level
/// contributes NO sibling — exact consumption, D-156).
pub fn merkle_proof(leaves: &[Bytes32], index: usize) -> Vec<Bytes32> {
    assert!(index < leaves.len());
    let mut proof = Vec::new();
    let mut level: Vec<Bytes32> = leaves.to_vec();
    let mut idx = index;
    while level.len() > 1 {
        if idx % 2 == 1 {
            proof.push(level[idx - 1]);
        } else if idx + 1 < level.len() {
            proof.push(level[idx + 1]);
        } // else: the trailing odd node promotes — no sibling
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            if pair.len() == 2 {
                let mut cat = [0u8; 64];
                cat[..32].copy_from_slice(&pair[0]);
                cat[32..].copy_from_slice(&pair[1]);
                next.push(h_tag(Tag::Bnode, &cat));
            } else {
                next.push(pair[0]);
            }
        }
        level = next;
        idx /= 2;
    }
    proof
}

/// Fold a leaf up its path — siblings bottom-up, exact consumption
/// (leftover or missing siblings fail → `None`), odd nodes promoting
/// unchanged (§11.8). The layout derives from `rec_index` +
/// `record_count` (both signed release facts).
pub fn fold_proof(
    leaf: Bytes32,
    rec_index: u64,
    record_count: u64,
    proof: &[Bytes32],
) -> Option<Bytes32> {
    if record_count == 0 || rec_index >= record_count {
        return None;
    }
    let mut node = leaf;
    let mut idx = rec_index;
    let mut size = record_count;
    let mut cursor = 0usize;
    while size > 1 {
        if idx % 2 == 1 {
            let sib = proof.get(cursor)?;
            cursor += 1;
            let mut cat = [0u8; 64];
            cat[..32].copy_from_slice(sib);
            cat[32..].copy_from_slice(&node);
            node = h_tag(Tag::Bnode, &cat);
        } else if idx + 1 < size {
            let sib = proof.get(cursor)?;
            cursor += 1;
            let mut cat = [0u8; 64];
            cat[..32].copy_from_slice(&node);
            cat[32..].copy_from_slice(sib);
            node = h_tag(Tag::Bnode, &cat);
        } // else: promote unchanged, consume nothing
        idx /= 2;
        size = size.div_ceil(2);
    }
    (cursor == proof.len()).then_some(node)
}

/// `evref = { ns: "plane", op: bytes32, zone: ulid, ? plane_id: bytes32,
///   ? span: text, class_floor: class }
/// / { ns: "external", scheme: "session-log" / "url" / "file",
///   locator_hash: bytes32, digest: bytes32, class_floor: class }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evref {
    Plane {
        op: Bytes32,
        zone: Bytes16,
        plane_id: Option<Bytes32>,
        span: Option<String>,
        class_floor: Class,
    },
    External {
        scheme: ExternalScheme,
        locator_hash: Bytes32,
        digest: Bytes32,
        class_floor: Class,
    },
}

closed_vocab!(ExternalScheme {
    SessionLog => "session-log",
    Url => "url",
    File => "file",
});

impl ToValue for Evref {
    fn to_value(&self) -> Value {
        match self {
            Evref::Plane {
                op,
                zone,
                plane_id,
                span,
                class_floor,
            } => {
                let mut entries = vec![
                    ("ns", text("plane")),
                    ("op", bytes(op)),
                    ("zone", bytes(zone)),
                ];
                if let Some(p) = plane_id {
                    entries.push(("plane_id", bytes(p)));
                }
                if let Some(s) = span {
                    entries.push(("span", text(s)));
                }
                entries.push(("class_floor", class_floor.to_value()));
                cbor::map(entries)
            }
            Evref::External {
                scheme,
                locator_hash,
                digest,
                class_floor,
            } => cbor::map(vec![
                ("ns", text("external")),
                ("scheme", scheme.to_value()),
                ("locator_hash", bytes(locator_hash)),
                ("digest", bytes(digest)),
                ("class_floor", class_floor.to_value()),
            ]),
        }
    }
}

/// `mclaim = { kind, statement, sensitivity, ? observed_at_ms,
///   ? valid_from_ms, ? valid_until_ms, ? expires_at_ms,
///   provenance: { ? session, ? project, ? model, evidence: [* evref] },
///   ? supersedes: [* bytes32], ? labels: [* text] }` — no import arm
/// (D-151: imports ride m.import.claim exclusively).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mclaim {
    pub kind: Kind,
    pub statement: String,
    pub sensitivity: Class,
    pub observed_at_ms: Option<u64>,
    pub valid_from_ms: Option<u64>,
    pub valid_until_ms: Option<u64>,
    pub expires_at_ms: Option<u64>,
    pub session: Option<String>,
    pub project: Option<String>,
    pub model: Option<String>,
    pub evidence: Vec<Evref>,
    pub supersedes: Option<Vec<Bytes32>>,
    pub labels: Option<Vec<String>>,
}

impl Mclaim {
    pub const OP_TYPE: &'static str = "m.claim";
}

impl ToValue for Mclaim {
    fn to_value(&self) -> Value {
        let mut entries = vec![
            ("kind", self.kind.to_value()),
            ("statement", text(&self.statement)),
            ("sensitivity", self.sensitivity.to_value()),
        ];
        if let Some(v) = self.observed_at_ms {
            entries.push(("observed_at_ms", u(v)));
        }
        if let Some(v) = self.valid_from_ms {
            entries.push(("valid_from_ms", u(v)));
        }
        if let Some(v) = self.valid_until_ms {
            entries.push(("valid_until_ms", u(v)));
        }
        if let Some(v) = self.expires_at_ms {
            entries.push(("expires_at_ms", u(v)));
        }
        let mut prov = Vec::new();
        if let Some(s) = &self.session {
            prov.push(("session", text(s)));
        }
        if let Some(p) = &self.project {
            prov.push(("project", text(p)));
        }
        if let Some(m) = &self.model {
            prov.push(("model", text(m)));
        }
        prov.push((
            "evidence",
            Value::Array(self.evidence.iter().map(|e| e.to_value()).collect()),
        ));
        entries.push(("provenance", cbor::map(prov)));
        if let Some(ss) = &self.supersedes {
            entries.push((
                "supersedes",
                Value::Array(ss.iter().map(|s| bytes(s)).collect()),
            ));
        }
        if let Some(ls) = &self.labels {
            entries.push(("labels", Value::Array(ls.iter().map(|l| text(l)).collect())));
        }
        cbor::map(entries)
    }
}

closed_vocab!(BasicVerdict {
    Accept => "accept",
    Dispute => "dispute",
    Retract => "retract",
    Retire => "retire",
});

closed_vocab!(ClassVerdict {
    RaiseClass => "raise_class",
    Declassify => "declassify",
});

/// `mjudge` — three arms discriminated by verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mjudge {
    Basic {
        verdict: BasicVerdict,
        target: Bytes32,
        policy: Polref,
        reason: Option<String>,
        evidence: Option<Vec<Evref>>,
    },
    Supersede {
        target: Bytes32,
        replacement: Bytes32,
        policy: Polref,
        reason: Option<String>,
    },
    Class {
        verdict: ClassVerdict,
        target: Bytes32,
        new_class: Class,
        policy: Polref,
        reason: Option<String>,
    },
}

impl Mjudge {
    pub const OP_TYPE: &'static str = "m.judge";
}

impl ToValue for Mjudge {
    fn to_value(&self) -> Value {
        match self {
            Mjudge::Basic {
                verdict,
                target,
                policy,
                reason,
                evidence,
            } => {
                let mut entries = vec![
                    ("verdict", verdict.to_value()),
                    ("target", bytes(target)),
                    ("policy", policy.to_value()),
                ];
                if let Some(r) = reason {
                    entries.push(("reason", text(r)));
                }
                if let Some(ev) = evidence {
                    entries.push((
                        "evidence",
                        Value::Array(ev.iter().map(|e| e.to_value()).collect()),
                    ));
                }
                cbor::map(entries)
            }
            Mjudge::Supersede {
                target,
                replacement,
                policy,
                reason,
            } => {
                let mut entries = vec![
                    ("verdict", text("supersede")),
                    ("target", bytes(target)),
                    ("replacement", bytes(replacement)),
                    ("policy", policy.to_value()),
                ];
                if let Some(r) = reason {
                    entries.push(("reason", text(r)));
                }
                cbor::map(entries)
            }
            Mjudge::Class {
                verdict,
                target,
                new_class,
                policy,
                reason,
            } => {
                let mut entries = vec![
                    ("verdict", verdict.to_value()),
                    ("target", bytes(target)),
                    ("new_class", new_class.to_value()),
                    ("policy", policy.to_value()),
                ];
                if let Some(r) = reason {
                    entries.push(("reason", text(r)));
                }
                cbor::map(entries)
            }
        }
    }
}

/// `mpin = { target: bytes32, destination: { space: ulid, role: text },
///   ? expiry_ms: ms, ? token_budget: uint, ? provenance_floor: class,
///   accepted_under: { judgment: bytes32, policy: polref } }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mpin {
    pub target: Bytes32,
    pub dest_space: Bytes16,
    pub dest_role: String,
    pub expiry_ms: Option<u64>,
    pub token_budget: Option<u64>,
    pub provenance_floor: Option<Class>,
    pub accepted_under_judgment: Bytes32,
    pub accepted_under_policy: Polref,
}

impl Mpin {
    pub const OP_TYPE: &'static str = "m.pin";
}

impl ToValue for Mpin {
    fn to_value(&self) -> Value {
        let mut entries = vec![
            ("target", bytes(&self.target)),
            (
                "destination",
                cbor::map(vec![
                    ("space", bytes(&self.dest_space)),
                    ("role", text(&self.dest_role)),
                ]),
            ),
        ];
        if let Some(e) = self.expiry_ms {
            entries.push(("expiry_ms", u(e)));
        }
        if let Some(t) = self.token_budget {
            entries.push(("token_budget", u(t)));
        }
        if let Some(p) = self.provenance_floor {
            entries.push(("provenance_floor", p.to_value()));
        }
        entries.push((
            "accepted_under",
            cbor::map(vec![
                ("judgment", bytes(&self.accepted_under_judgment)),
                ("policy", self.accepted_under_policy.to_value()),
            ]),
        ));
        cbor::map(entries)
    }
}

/// `munpin = { target_pin: bytes32 }`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Munpin {
    pub target_pin: Bytes32,
}

impl Munpin {
    pub const OP_TYPE: &'static str = "m.unpin";
}

impl ToValue for Munpin {
    fn to_value(&self) -> Value {
        cbor::map(vec![("target_pin", bytes(&self.target_pin))])
    }
}

/// `merasereq = { targets: [+ bytes32] }` — set (E7): claim op
/// hashes (D-66), ≤ 128 (E8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Merasereq {
    pub targets: Vec<Bytes32>,
}

impl Merasereq {
    pub const OP_TYPE: &'static str = "m.erase_request";
}

impl ToValue for Merasereq {
    fn to_value(&self) -> Value {
        assert!(!self.targets.is_empty(), "merasereq.targets is non-empty");
        cbor::map(vec![(
            "targets",
            Value::Array(sorted_set_default(
                self.targets.iter().map(|t| bytes(t)).collect(),
                "merasereq.targets",
            )),
        )])
    }
}

/// `mexportrel = { export_id, sources: [+ bytes32], content_digest,
///   to: endpoint, class_floor, data_frontier, control_frontier,
///   as_of_ms, expiry_deadline_ms }` — sources: set keyed op, claims
/// only, ≤ 128; content_digest = the bundle MERKLE ROOT (§11.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mexportrel {
    pub export_id: Bytes16,
    pub sources: Vec<Bytes32>,
    pub content_digest: Bytes32,
    pub to: Endpoint,
    pub class_floor: Class,
    pub data_frontier: Bytes32,
    pub control_frontier: Bytes32,
    pub as_of_ms: u64,
    pub expiry_deadline_ms: u64,
}

impl Mexportrel {
    pub const OP_TYPE: &'static str = "m.export.release";
}

impl ToValue for Mexportrel {
    fn to_value(&self) -> Value {
        assert!(!self.sources.is_empty(), "mexportrel.sources is non-empty");
        cbor::map(vec![
            ("export_id", bytes(&self.export_id)),
            (
                "sources",
                Value::Array(sorted_set_default(
                    self.sources.iter().map(|s| bytes(s)).collect(),
                    "mexportrel.sources",
                )),
            ),
            ("content_digest", bytes(&self.content_digest)),
            ("to", self.to.to_value()),
            ("class_floor", self.class_floor.to_value()),
            ("data_frontier", bytes(&self.data_frontier)),
            ("control_frontier", bytes(&self.control_frontier)),
            ("as_of_ms", u(self.as_of_ms)),
            ("expiry_deadline_ms", u(self.expiry_deadline_ms)),
        ])
    }
}

/// `mimport = { source_op, class_floor, kind, statement, sensitivity,
///   rec_index, proof: [* bytes32], provenance: { import: { from_plane,
///   export_id, release_op, digest } } }` — the DEDICATED narrow shape
/// (D-142): temporal/session/labels/evidence are STRUCTURALLY absent;
/// sensitivity == class_floor (D-134); proof ≤ 7 (E8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mimport {
    pub source_op: Bytes32,
    pub class_floor: Class,
    pub kind: Kind,
    pub statement: String,
    pub sensitivity: Class,
    pub rec_index: u64,
    pub proof: Vec<Bytes32>,
    pub from_plane: Bytes32,
    pub export_id: Bytes16,
    pub release_op: Bytes32,
    pub digest: Bytes32,
}

impl Mimport {
    pub const OP_TYPE: &'static str = "m.import.claim";
}

impl ToValue for Mimport {
    fn to_value(&self) -> Value {
        assert!(self.proof.len() <= 7, "mimport.proof <= 7 (E8)");
        cbor::map(vec![
            ("source_op", bytes(&self.source_op)),
            ("class_floor", self.class_floor.to_value()),
            ("kind", self.kind.to_value()),
            ("statement", text(&self.statement)),
            ("sensitivity", self.sensitivity.to_value()),
            ("rec_index", u(self.rec_index)),
            (
                "proof",
                Value::Array(self.proof.iter().map(|p| bytes(p)).collect()),
            ),
            (
                "provenance",
                cbor::map(vec![(
                    "import",
                    cbor::map(vec![
                        ("from_plane", bytes(&self.from_plane)),
                        ("export_id", bytes(&self.export_id)),
                        ("release_op", bytes(&self.release_op)),
                        ("digest", bytes(&self.digest)),
                    ]),
                )]),
            ),
        ])
    }
}

/// `auditprin` — the five closed principal shapes (D-74/D-83).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auditprin {
    Device {
        device: Bytes16,
    },
    DeviceSession {
        device: Bytes16,
        session: String,
    },
    Token {
        token_hash: Bytes32,
    },
    Peer {
        peer: String,
        token_hash: Option<Bytes32>,
    },
    MediatedSession {
        session: String,
        token_hash: Bytes32,
    },
}

impl ToValue for Auditprin {
    fn to_value(&self) -> Value {
        match self {
            Auditprin::Device { device } => {
                cbor::map(vec![("shape", u(1)), ("device", bytes(device))])
            }
            Auditprin::DeviceSession { device, session } => cbor::map(vec![
                ("shape", u(2)),
                ("device", bytes(device)),
                ("session", text(session)),
            ]),
            Auditprin::Token { token_hash } => {
                cbor::map(vec![("shape", u(3)), ("token_hash", bytes(token_hash))])
            }
            Auditprin::Peer { peer, token_hash } => {
                let mut entries = vec![
                    ("shape", u(4)),
                    ("kind", text("peer")),
                    ("peer", text(peer)),
                ];
                if let Some(t) = token_hash {
                    entries.push(("token_hash", bytes(t)));
                }
                cbor::map(entries)
            }
            Auditprin::MediatedSession {
                session,
                token_hash,
            } => cbor::map(vec![
                ("shape", u(4)),
                ("kind", text("session")),
                ("session", text(session)),
                ("token_hash", bytes(token_hash)),
            ]),
        }
    }
}

/// `maudit = { principal: auditprin, read_id: bytes16,
///   chunk: { index: uint, count: uint },
///   scope: { zone: ulid, spaces: [+ ulid] },
///   result_ids: [* bytes32], at_ms: ms }` — spaces set ≤ 64, one
/// read = one zone (D-83); result_ids set (op hashes, D-74).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Maudit {
    pub principal: Auditprin,
    pub read_id: Bytes16,
    pub chunk_index: u64,
    pub chunk_count: u64,
    pub scope_zone: Bytes16,
    pub scope_spaces: Vec<Bytes16>,
    pub result_ids: Vec<Bytes32>,
    pub at_ms: u64,
}

impl Maudit {
    pub const OP_TYPE: &'static str = "m.audit";
}

impl ToValue for Maudit {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("principal", self.principal.to_value()),
            ("read_id", bytes(&self.read_id)),
            (
                "chunk",
                cbor::map(vec![
                    ("index", u(self.chunk_index)),
                    ("count", u(self.chunk_count)),
                ]),
            ),
            (
                "scope",
                cbor::map(vec![
                    ("zone", bytes(&self.scope_zone)),
                    (
                        "spaces",
                        Value::Array(sorted_set_default(
                            self.scope_spaces.iter().map(|s| bytes(s)).collect(),
                            "maudit.scope.spaces",
                        )),
                    ),
                ]),
            ),
            (
                "result_ids",
                Value::Array(sorted_set_default(
                    self.result_ids.iter().map(|r| bytes(r)).collect(),
                    "maudit.result_ids",
                )),
            ),
            ("at_ms", u(self.at_ms)),
        ])
    }
}

/// `wgen = { last_known: { gen: uint, seq: uint, op: bytes32 } / "unknown" }`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wgen {
    LastKnown { gen: u64, seq: u64, op: Bytes32 },
    Unknown,
}

impl Wgen {
    pub const OP_TYPE: &'static str = "w.gen";
}

impl ToValue for Wgen {
    fn to_value(&self) -> Value {
        let last_known = match self {
            Wgen::LastKnown { gen, seq, op } => {
                cbor::map(vec![("gen", u(*gen)), ("seq", u(*seq)), ("op", bytes(op))])
            }
            Wgen::Unknown => text("unknown"),
        };
        cbor::map(vec![("last_known", last_known)])
    }
}

// ---- A.6 policy object (§11.3) ----

closed_vocab!(Verdictname {
    Accept => "accept",
    Dispute => "dispute",
    Retract => "retract",
    Retire => "retire",
    Supersede => "supersede",
    RaiseClass => "raise_class",
    Declassify => "declassify",
});

closed_vocab!(ActorClass {
    Owner => "owner",
    SafeHuman => "safe-human",
    Session => "session",
    External => "external",
    Peer => "peer",
    Service => "service",
});

closed_vocab!(Relation {
    SelfP => "self",
    Author => "author",
    Any => "any",
});

/// `kinds: [+ kind] / "*"` (and its spaceclass twin).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindsSel {
    Kinds(Vec<Kind>),
    Wildcard,
}

impl ToValue for KindsSel {
    fn to_value(&self) -> Value {
        match self {
            KindsSel::Kinds(ks) => Value::Array(ks.iter().map(|k| k.to_value()).collect()),
            KindsSel::Wildcard => text("*"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpaceClassesSel {
    Classes(Vec<Spaceclass>),
    Wildcard,
}

impl ToValue for SpaceClassesSel {
    fn to_value(&self) -> Value {
        match self {
            SpaceClassesSel::Classes(cs) => Value::Array(cs.iter().map(|c| c.to_value()).collect()),
            SpaceClassesSel::Wildcard => text("*"),
        }
    }
}

/// `rule = { verdict: verdictname, kinds: [+ kind] / "*",
///   space_classes: [+ spaceclass] / "*", actor_classes: [+ (...)],
///   relation: "self" / "author" / "any" }` — relation REQUIRED
/// (D-51, principal-level §11.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub verdict: Verdictname,
    pub kinds: KindsSel,
    pub space_classes: SpaceClassesSel,
    pub actor_classes: Vec<ActorClass>,
    pub relation: Relation,
}

impl ToValue for Rule {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("verdict", self.verdict.to_value()),
            ("kinds", self.kinds.to_value()),
            ("space_classes", self.space_classes.to_value()),
            (
                "actor_classes",
                Value::Array(self.actor_classes.iter().map(|a| a.to_value()).collect()),
            ),
            ("relation", self.relation.to_value()),
        ])
    }
}

/// `policy = { v: 1, policy_id: text, version: uint, rules: [+ rule] }`
/// — rules: set (E7), sorted (default key), duplicate-free;
/// `H_policy` over the canonical bytes is the polref hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub policy_id: String,
    pub version: u64,
    pub rules: Vec<Rule>,
}

impl ToValue for Policy {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("v", u(1)),
            ("policy_id", text(&self.policy_id)),
            ("version", u(self.version)),
            (
                "rules",
                Value::Array(sorted_set_default(
                    self.rules.iter().map(|r| r.to_value()).collect(),
                    "policy.rules",
                )),
            ),
        ])
    }
}

impl Policy {
    /// The `polref.hash` — `H_policy(canonical policy)`.
    pub fn hash(&self) -> Bytes32 {
        h_tag(
            Tag::Policy,
            &cbor::encode(&self.to_value()).expect("policy encodes"),
        )
    }
}

#[cfg(test)]
pub(crate) const CDDL_PINS_MEMORY: &[&str] = &[
    r#"bundlerec = { v: 1, op: bytes32, kind: kind, statement: text,
  class_floor: class }"#,
    r#"bundleleaf = { v: 1, export_id: bytes16, rec_index: uint,
  rec: bundlerec }"#,
    r#"bundle = { v: 1, export_id: bytes16, recs: [+ bundlerec] }"#,
    r#"evref = { ns: "plane", op: bytes32, zone: ulid, ? plane_id: bytes32,
          ? span: text, class_floor: class }
      / { ns: "external", scheme: "session-log" / "url" / "file",
          locator_hash: bytes32, digest: bytes32, class_floor: class }"#,
    r#"mclaim = { kind: kind, statement: text, sensitivity: class,
  ? observed_at_ms: ms, ? valid_from_ms: ms, ? valid_until_ms: ms,
  ? expires_at_ms: ms,
  provenance: { ? session: text, ? project: text, ? model: text,
    evidence: [* evref] },"#,
    r#"  ? supersedes: [* bytes32],   ; advisory lineage links (§11.2)
  ? labels: [* text] }"#,
    r#"mjudge = { verdict: "accept" / "dispute" / "retract" / "retire",
           target: bytes32, policy: polref, ? reason: text,
           ? evidence: [* evref] }
       / { verdict: "supersede", target: bytes32, replacement: bytes32,
           policy: polref, ? reason: text }
       / { verdict: "raise_class" / "declassify", target: bytes32,
           new_class: class, policy: polref, ? reason: text }"#,
    r#"mpin = { target: bytes32, destination: { space: ulid, role: text },
  ? expiry_ms: ms, ? token_budget: uint, ? provenance_floor: class,
  accepted_under: { judgment: bytes32, policy: polref } }
munpin = { target_pin: bytes32 }"#,
    r#"merasereq = { targets: [+ bytes32] }"#,
    r#"mexportrel = { export_id: bytes16,
  sources: [+ bytes32],     ; set (E7), keyed by op; claims only"#,
    r#"  to: endpoint, class_floor: class,
  data_frontier: bytes32, control_frontier: bytes32, as_of_ms: ms,"#,
    r#"  expiry_deadline_ms: ms }"#,
    r#"mimport = { source_op: bytes32,   ; the released bundlerec this binds to
  class_floor: class,               ; the immutable lower bound
  kind: kind, statement: text,
  sensitivity: class,               ; == class_floor (equality, D-134)"#,
    r#"  provenance: {
    import: { from_plane: bytes32, export_id: bytes16,
              release_op: bytes32,
              digest: bytes32 } } }"#,
    r#"auditprin = { shape: 1, device: bytes16 }
          / { shape: 2, device: bytes16, session: text }
          / { shape: 3, token_hash: bytes32 }
          / { shape: 4, kind: "peer", peer: text,
              ? token_hash: bytes32 }
          / { shape: 4, kind: "session", session: text,
              token_hash: bytes32 }"#,
    r#"maudit = { principal: auditprin, read_id: bytes16,
  chunk: { index: uint, count: uint },
  scope: { zone: ulid, spaces: [+ ulid] },"#,
    r#"wgen = { last_known: { gen: uint, seq: uint, op: bytes32 } / "unknown" }"#,
    r#"verdictname = "accept" / "dispute" / "retract" / "retire"
            / "supersede" / "raise_class" / "declassify"
policy = { v: 1, policy_id: text, version: uint,
  rules: [+ rule] }"#,
    r#"rule = { verdict: verdictname, kinds: [+ kind] / "*",
         space_classes: [+ spaceclass] / "*",
         actor_classes: [+ ("owner" / "safe-human" / "session"
                           / "external" / "peer" / "service")],
         relation: "self" / "author" / "any" }"#,
    r#"  leaf_i = H_brec(canonical bundleleaf_i)"#,
    r#"  parents = H_bnode(left || right); an odd node promotes unchanged"#,
];

#[cfg(test)]
mod tests {
    use super::super::{assert_pins, map_keys, spec_text};
    use super::*;

    fn rec(op_byte: u8) -> Bundlerec {
        Bundlerec {
            op: [op_byte; 32],
            kind: Kind::Observation,
            statement: format!("statement {op_byte}"),
            class_floor: Class::Private,
        }
    }

    #[test]
    fn cddl_pins_are_verbatim() {
        assert_pins(CDDL_PINS_MEMORY);
    }

    #[test]
    fn memory_op_types_exist_in_the_registry() {
        let spec = spec_text();
        for op in [
            Mclaim::OP_TYPE,
            Mjudge::OP_TYPE,
            Mpin::OP_TYPE,
            Munpin::OP_TYPE,
            Merasereq::OP_TYPE,
            Mexportrel::OP_TYPE,
            Mimport::OP_TYPE,
            Maudit::OP_TYPE,
            Wgen::OP_TYPE,
        ] {
            assert!(
                spec.contains(&format!("`{op}`")),
                "operation_type `{op}` not found in the spec registry"
            );
        }
    }

    #[test]
    fn merkle_roundtrip_every_size_and_index() {
        // Sizes crossing the odd-promotion cases; every index proves.
        for n in 1..=9usize {
            let leaves: Vec<[u8; 32]> = (0..n).map(|i| [i as u8 + 1; 32]).collect();
            let root = merkle_root(&leaves);
            for (i, leaf) in leaves.iter().enumerate() {
                let proof = merkle_proof(&leaves, i);
                assert_eq!(
                    fold_proof(*leaf, i as u64, n as u64, &proof),
                    Some(root),
                    "size {n} index {i}"
                );
                // Exact consumption: an extra sibling fails...
                let mut long = proof.clone();
                long.push([0xee; 32]);
                assert_eq!(fold_proof(*leaf, i as u64, n as u64, &long), None);
                // ...and a missing one fails (when any were needed).
                if !proof.is_empty() {
                    let short = &proof[..proof.len() - 1];
                    assert_eq!(fold_proof(*leaf, i as u64, n as u64, short), None);
                }
            }
        }
        // A single record is its own root.
        assert_eq!(merkle_root(&[[7; 32]]), [7; 32]);
        // 128 records → depth-7 proofs (the E8 cap is exact).
        let leaves: Vec<[u8; 32]> = (0..128).map(|i| [i as u8; 32]).collect();
        assert_eq!(merkle_proof(&leaves, 0).len(), 7);
    }

    #[test]
    fn bundle_digest_binds_export_and_rank() {
        let b = Bundle {
            export_id: [1; 16],
            recs: vec![rec(5), rec(2), rec(9)],
        };
        // Leaves follow the op-sorted rank, self-describing.
        let leaves = b.leaves();
        let expected0 = Bundleleaf {
            export_id: [1; 16],
            rec_index: 0,
            rec: rec(2),
        }
        .leaf_hash();
        assert_eq!(leaves[0], expected0);
        let root = b.content_digest();
        // A per-record proof folds to the root.
        let proof = merkle_proof(&leaves, 2);
        assert_eq!(fold_proof(leaves[2], 2, 3, &proof), Some(root));
        // A different export_id shifts every leaf.
        let other = Bundle {
            export_id: [2; 16],
            ..b.clone()
        };
        assert_ne!(root, other.content_digest());
        // recs are an op-keyed set — duplicates panic.
        let dup = Bundle {
            export_id: [1; 16],
            recs: vec![rec(2), rec(2)],
        };
        assert!(std::panic::catch_unwind(move || dup.to_value()).is_err());
    }

    #[test]
    fn mimport_is_narrow_and_capped() {
        let m = Mimport {
            source_op: [1; 32],
            class_floor: Class::Private,
            kind: Kind::Decision,
            statement: "imported".into(),
            sensitivity: Class::Private,
            rec_index: 0,
            proof: vec![[2; 32]],
            from_plane: [3; 32],
            export_id: [4; 16],
            release_op: [5; 32],
            digest: [6; 32],
        };
        assert_eq!(
            map_keys(&m.to_value()),
            [
                "source_op",
                "class_floor",
                "kind",
                "statement",
                "sensitivity",
                "rec_index",
                "proof",
                "provenance"
            ]
        );
        let over = Mimport {
            proof: vec![[0; 32]; 8],
            ..m
        };
        assert!(std::panic::catch_unwind(move || over.to_value()).is_err());
    }

    #[test]
    fn mclaim_and_judgment_shapes() {
        let claim = Mclaim {
            kind: Kind::Observation,
            statement: "s".into(),
            sensitivity: Class::Internal,
            observed_at_ms: None,
            valid_from_ms: None,
            valid_until_ms: None,
            expires_at_ms: None,
            session: Some("sess".into()),
            project: None,
            model: None,
            evidence: vec![],
            supersedes: None,
            labels: None,
        };
        assert_eq!(
            map_keys(&claim.to_value()),
            ["kind", "statement", "sensitivity", "provenance"]
        );
        let Value::Map(entries) = claim.to_value() else {
            panic!()
        };
        let prov = &entries
            .iter()
            .find(|(k, _)| *k == Value::Text("provenance".into()))
            .unwrap()
            .1;
        assert_eq!(map_keys(prov), ["session", "evidence"]);

        let judge = Mjudge::Supersede {
            target: [1; 32],
            replacement: [2; 32],
            policy: Polref {
                id: "p".into(),
                version: 1,
                hash: [3; 32],
            },
            reason: None,
        };
        assert_eq!(
            map_keys(&judge.to_value()),
            ["verdict", "target", "replacement", "policy"]
        );
    }

    #[test]
    fn audit_and_wgen_shapes() {
        let audit = Maudit {
            principal: Auditprin::MediatedSession {
                session: "s1".into(),
                token_hash: [1; 32],
            },
            read_id: [2; 16],
            chunk_index: 0,
            chunk_count: 1,
            scope_zone: [3; 16],
            scope_spaces: vec![[9; 16], [4; 16]],
            result_ids: vec![[8; 32], [5; 32]],
            at_ms: 100,
        };
        let v = audit.to_value();
        assert_eq!(
            map_keys(&v),
            [
                "principal",
                "read_id",
                "chunk",
                "scope",
                "result_ids",
                "at_ms"
            ]
        );
        let Value::Map(entries) = &v else { panic!() };
        let principal = &entries[0].1;
        assert_eq!(
            map_keys(principal),
            ["shape", "kind", "session", "token_hash"]
        );
        assert_eq!(
            map_keys(
                &Wgen::LastKnown {
                    gen: 1,
                    seq: 2,
                    op: [1; 32]
                }
                .to_value()
            ),
            ["last_known"]
        );
        let Value::Map(w) = Wgen::Unknown.to_value() else {
            panic!()
        };
        assert_eq!(w[0].1, Value::Text("unknown".into()));
    }

    #[test]
    fn policy_rules_sort_and_hash() {
        let rule = |verdict: Verdictname| Rule {
            verdict,
            kinds: KindsSel::Wildcard,
            space_classes: SpaceClassesSel::Wildcard,
            actor_classes: vec![ActorClass::Owner],
            relation: Relation::Any,
        };
        let p1 = Policy {
            policy_id: "workflow-v1".into(),
            version: 1,
            rules: vec![rule(Verdictname::Supersede), rule(Verdictname::Accept)],
        };
        let p2 = Policy {
            policy_id: "workflow-v1".into(),
            version: 1,
            rules: vec![rule(Verdictname::Accept), rule(Verdictname::Supersede)],
        };
        // Rule order is canonicalized, so the hash is order-independent.
        assert_eq!(p1.hash(), p2.hash());
        assert_eq!(Verdictname::ALL.len(), 7);
        assert_eq!(ActorClass::ALL.len(), 6);
    }
}
