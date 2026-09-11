//! Appendix A.4 — item/storage frames, the transfer journal records
//! (§6.1), the zone Frontier (§4.6), and signed receipts/leases
//! (§4.7).
//!
//! Frame-type IDs are §6.1's: one constant per frame; the framing
//! WRAPPER (file framing v2, §6.2) is a vector-builder concern — this
//! module shapes the frame BODY objects.

use super::envelope::OpSigner;
use super::{
    bytes, key_bytes, sorted_set_by_key, sorted_set_default, text, u, Bytes16, Bytes32, Factref,
    Head, Issuerid, Opfactref, ToValue,
};
use crate::cbor::{self, Value};
use crate::domains::{h_tag, Tag};
use crate::suite;

/// §6.1 frame-type IDs (0x01 is control-log only).
pub const FRAME_CTRL_OP: u8 = 0x01;
pub const FRAME_ITEM_COMMIT: u8 = 0x11;
pub const FRAME_ITEM_REWRAP: u8 = 0x12;
pub const FRAME_FENCE: u8 = 0x13;
pub const FRAME_REWRAP_DONE: u8 = 0x14;
pub const FRAME_TOMBSTONE: u8 = 0x15;
pub const FRAME_RECEIPT: u8 = 0x16;
pub const FRAME_OUTBOX_MARK: u8 = 0x17;
pub const FRAME_TXN: u8 = 0x18;
pub const FRAME_KEK_DESTROYED: u8 = 0x1a;

/// `frontier = { v: 1, zone_id: ulid, heads: [* head] }` — explicit
/// E7 sort key `(lineage, gen)`, at most one head per pair (§4.6);
/// `H_frontier` over the canonical bytes is its identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontier {
    pub zone_id: Bytes16,
    pub heads: Vec<Head>,
}

impl ToValue for Frontier {
    fn to_value(&self) -> Value {
        let heads = sorted_set_by_key(
            self.heads
                .iter()
                .map(|h| (key_bytes(&[bytes(&h.lineage), u(h.gen)]), h.to_value()))
                .collect(),
            "frontier.heads (key lineage, gen)",
        );
        cbor::map(vec![
            ("v", u(1)),
            ("zone_id", bytes(&self.zone_id)),
            ("heads", Value::Array(heads)),
        ])
    }
}

impl Frontier {
    /// `H_frontier(canonical frontier)` — the frontier identity.
    pub fn hash(&self) -> Bytes32 {
        h_tag(
            Tag::Frontier,
            &cbor::encode(&self.to_value()).expect("frontier encodes"),
        )
    }
}

/// `ctrlopframe = { op: bstr }` — 0x01: the exact SignedOperation
/// triple bytes (control log only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ctrlopframe {
    pub op: Vec<u8>,
}

impl ToValue for Ctrlopframe {
    fn to_value(&self) -> Value {
        cbor::map(vec![("op", bytes(&self.op))])
    }
}

/// `itemcore = { v: 1, aead: "a256gcm", nonce: bstr .size 12, ct: bstr }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Itemcore {
    pub nonce: [u8; 12],
    pub ct: Vec<u8>,
}

impl ToValue for Itemcore {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("v", u(1)),
            ("aead", text("a256gcm")),
            ("nonce", bytes(&self.nonce)),
            ("ct", bytes(&self.ct)),
        ])
    }
}

/// `itemwrap = { v: 1, item_addr: bytes32, key_wrap_epoch: uint,
///   wrapped_dek: bstr .size 48 }` — AES-256-GCM(32-byte DEK): ct‖tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Itemwrap {
    pub item_addr: Bytes32,
    pub key_wrap_epoch: u64,
    pub wrapped_dek: [u8; 48],
}

impl ToValue for Itemwrap {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("v", u(1)),
            ("item_addr", bytes(&self.item_addr)),
            ("key_wrap_epoch", u(self.key_wrap_epoch)),
            ("wrapped_dek", bytes(&self.wrapped_dek)),
        ])
    }
}

/// `itemrewrapframe = { wrap: itemwrap }` — 0x12.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Itemrewrapframe {
    pub wrap: Itemwrap,
}

impl ToValue for Itemrewrapframe {
    fn to_value(&self) -> Value {
        cbor::map(vec![("wrap", self.wrap.to_value())])
    }
}

/// `fenceframe = { kek_epoch: uint, rotation_op: bytes32,
///   fence_frontier: bytes32, control_frontier: bytes32,
///   recipients_hash: bytes32 }` — 0x13 (D-77/D-97).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fenceframe {
    pub kek_epoch: u64,
    pub rotation_op: Bytes32,
    pub fence_frontier: Bytes32,
    pub control_frontier: Bytes32,
    pub recipients_hash: Bytes32,
}

impl ToValue for Fenceframe {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("kek_epoch", u(self.kek_epoch)),
            ("rotation_op", bytes(&self.rotation_op)),
            ("fence_frontier", bytes(&self.fence_frontier)),
            ("control_frontier", bytes(&self.control_frontier)),
            ("recipients_hash", bytes(&self.recipients_hash)),
        ])
    }
}

/// `recipientset = { v: 1, pairs: [* { device: bytes16,
///   kem_key: bytes32 }] }` — set keyed device, ≤ 256 (D-110/D-125);
/// `recipients_hash = H_recips(canonical recipientset)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipientset {
    pub pairs: Vec<(Bytes16, Bytes32)>,
}

impl ToValue for Recipientset {
    fn to_value(&self) -> Value {
        let pairs = sorted_set_by_key(
            self.pairs
                .iter()
                .map(|(device, kem_key)| {
                    (
                        key_bytes(&[bytes(device)]),
                        cbor::map(vec![("device", bytes(device)), ("kem_key", bytes(kem_key))]),
                    )
                })
                .collect(),
            "recipientset.pairs (key device)",
        );
        cbor::map(vec![("v", u(1)), ("pairs", Value::Array(pairs))])
    }
}

impl Recipientset {
    pub fn hash(&self) -> Bytes32 {
        h_tag(
            Tag::Recips,
            &cbor::encode(&self.to_value()).expect("recipientset encodes"),
        )
    }
}

/// `tombstone = { v: 1, item_addr: bytes32, erase_op: bytes32,
///   target_op: bytes32, retired_epoch: uint }` — 0x15.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tombstone {
    pub item_addr: Bytes32,
    pub erase_op: Bytes32,
    pub target_op: Bytes32,
    pub retired_epoch: u64,
}

impl ToValue for Tombstone {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("v", u(1)),
            ("item_addr", bytes(&self.item_addr)),
            ("erase_op", bytes(&self.erase_op)),
            ("target_op", bytes(&self.target_op)),
            ("retired_epoch", u(self.retired_epoch)),
        ])
    }
}

/// `itemcommit = { core: itemcore, wrap: itemwrap, lineage: bytes16,
///   gen: uint, seq: uint }` — 0x11, and a `txnrec` member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Itemcommit {
    pub core: Itemcore,
    pub wrap: Itemwrap,
    pub lineage: Bytes16,
    pub gen: u64,
    pub seq: u64,
}

impl ToValue for Itemcommit {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("core", self.core.to_value()),
            ("wrap", self.wrap.to_value()),
            ("lineage", bytes(&self.lineage)),
            ("gen", u(self.gen)),
            ("seq", u(self.seq)),
        ])
    }
}

/// `rewrapdone = { kek_epoch, rotation_op, count, fence_frontier,
///   control_frontier, recipients_hash, survivors }` — 0x14; the
/// Fence-field equalities are D-97; `count == |survivorset.pairs|`
/// (D-92); `survivors = H_survivors(canonical survivorset)` (D-77).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rewrapdone {
    pub kek_epoch: u64,
    pub rotation_op: Bytes32,
    pub count: u64,
    pub fence_frontier: Bytes32,
    pub control_frontier: Bytes32,
    pub recipients_hash: Bytes32,
    pub survivors: Bytes32,
}

impl ToValue for Rewrapdone {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("kek_epoch", u(self.kek_epoch)),
            ("rotation_op", bytes(&self.rotation_op)),
            ("count", u(self.count)),
            ("fence_frontier", bytes(&self.fence_frontier)),
            ("control_frontier", bytes(&self.control_frontier)),
            ("recipients_hash", bytes(&self.recipients_hash)),
            ("survivors", bytes(&self.survivors)),
        ])
    }
}

/// `survivorset = { v: 1, pairs: [* survivorpair] }` — set keyed
/// item_addr; `survivorpair = { item_addr: bytes32, wrap_hash: bytes32 }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Survivorset {
    pub pairs: Vec<(Bytes32, Bytes32)>,
}

impl ToValue for Survivorset {
    fn to_value(&self) -> Value {
        let pairs = sorted_set_by_key(
            self.pairs
                .iter()
                .map(|(item_addr, wrap_hash)| {
                    (
                        key_bytes(&[bytes(item_addr)]),
                        cbor::map(vec![
                            ("item_addr", bytes(item_addr)),
                            ("wrap_hash", bytes(wrap_hash)),
                        ]),
                    )
                })
                .collect(),
            "survivorset.pairs (key item_addr)",
        );
        cbor::map(vec![("v", u(1)), ("pairs", Value::Array(pairs))])
    }
}

impl Survivorset {
    pub fn hash(&self) -> Bytes32 {
        h_tag(
            Tag::Survivors,
            &cbor::encode(&self.to_value()).expect("survivorset encodes"),
        )
    }
}

/// `outboxmark = { through: { lineage: bytes16, gen: uint, seq: uint } }` — 0x17.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outboxmark {
    pub lineage: Bytes16,
    pub gen: u64,
    pub seq: u64,
}

impl ToValue for Outboxmark {
    fn to_value(&self) -> Value {
        cbor::map(vec![(
            "through",
            cbor::map(vec![
                ("lineage", bytes(&self.lineage)),
                ("gen", u(self.gen)),
                ("seq", u(self.seq)),
            ]),
        )])
    }
}

/// `kekdestroyed = { epoch: uint }` — 0x1A; the destroyed epoch =
/// new_epoch − 1 (D-92).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kekdestroyed {
    pub epoch: u64,
}

impl ToValue for Kekdestroyed {
    fn to_value(&self) -> Value {
        cbor::map(vec![("epoch", u(self.epoch))])
    }
}

// ---- the transfer journal (txnrec union, D-140/D-200) ----

/// `pendingxfer = { export_id: bytes16, release_op: bytes32,
///   dest_zone: ulid, content_digest: bytes32, record_count: uint }`
/// — release_op = the journal identity (D-119/D-123); export_id =
/// correlation only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pendingxfer {
    pub export_id: Bytes16,
    pub release_op: Bytes32,
    pub dest_zone: Bytes16,
    pub content_digest: Bytes32,
    pub record_count: u64,
}

impl ToValue for Pendingxfer {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("export_id", bytes(&self.export_id)),
            ("release_op", bytes(&self.release_op)),
            ("dest_zone", bytes(&self.dest_zone)),
            ("content_digest", bytes(&self.content_digest)),
            ("record_count", u(self.record_count)),
        ])
    }
}

/// `xferdone = { export_id: bytes16, release_op: bytes32,
///   incarnation: uint, completed: [* bytes32] }` — completed: set
/// (E7, op-hash key) = the bundle's exact record set (D-65);
/// incarnation = the self-describing interval (D-192).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xferdone {
    pub export_id: Bytes16,
    pub release_op: Bytes32,
    pub incarnation: u64,
    pub completed: Vec<Bytes32>,
}

impl ToValue for Xferdone {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("export_id", bytes(&self.export_id)),
            ("release_op", bytes(&self.release_op)),
            ("incarnation", u(self.incarnation)),
            (
                "completed",
                Value::Array(sorted_set_default(
                    self.completed.iter().map(|c| bytes(c)).collect(),
                    "xferdone.completed",
                )),
            ),
        ])
    }
}

/// `xferabort.reason` — the branch-relative terminal vocabulary
/// ("release-rejected" is the journal-cleanup abort outside the
/// finality gate, D-126).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortReason {
    SourceErased,
    RejectPermanent,
    ReleaseRejected,
}

impl AbortReason {
    pub fn as_str(self) -> &'static str {
        match self {
            AbortReason::SourceErased => "source-erased",
            AbortReason::RejectPermanent => "reject-permanent",
            AbortReason::ReleaseRejected => "release-rejected",
        }
    }
}

/// One `xferabort.missing` entry: `{ rec: bytes32, ? basis: opfactref }`
/// — basis = ONE writer-chosen sufficient branch-relative fact
/// (op-kind only, D-179/D-193); absence = the intrinsic/static/
/// source-erased table (basis presence IS the discriminator, D-200).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissingRec {
    pub rec: Bytes32,
    pub basis: Option<Opfactref>,
}

impl ToValue for MissingRec {
    fn to_value(&self) -> Value {
        let mut entries = vec![("rec", bytes(&self.rec))];
        if let Some(b) = &self.basis {
            entries.push(("basis", b.to_value()));
        }
        cbor::map(entries)
    }
}

/// `xferabort = { export_id, release_op, reason, incarnation,
///   missing: [+ { rec, ? basis }] }` — missing: NON-EMPTY set keyed
/// rec (an empty residue is XferDone, D-98); terminal (D-65).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xferabort {
    pub export_id: Bytes16,
    pub release_op: Bytes32,
    pub reason: AbortReason,
    pub incarnation: u64,
    pub missing: Vec<MissingRec>,
}

impl ToValue for Xferabort {
    fn to_value(&self) -> Value {
        assert!(
            !self.missing.is_empty(),
            "xferabort.missing is non-empty (D-98: an empty residue is XferDone)"
        );
        let missing = sorted_set_by_key(
            self.missing
                .iter()
                .map(|m| (key_bytes(&[bytes(&m.rec)]), m.to_value()))
                .collect(),
            "xferabort.missing (key rec)",
        );
        cbor::map(vec![
            ("export_id", bytes(&self.export_id)),
            ("release_op", bytes(&self.release_op)),
            ("reason", text(self.reason.as_str())),
            ("incarnation", u(self.incarnation)),
            ("missing", Value::Array(missing)),
        ])
    }
}

/// `xferreopen = { export_id, release_op, incarnation,
///   basis: opfactref, invalidation: factref }` — basis = the
/// invalidated recorded cause (op-kind only, D-193/D-200);
/// invalidation = the killing fact (either domain); incarnation =
/// the terminal interval it closes (0-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Xferreopen {
    pub export_id: Bytes16,
    pub release_op: Bytes32,
    pub incarnation: u64,
    pub basis: Opfactref,
    pub invalidation: Factref,
}

impl ToValue for Xferreopen {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("export_id", bytes(&self.export_id)),
            ("release_op", bytes(&self.release_op)),
            ("incarnation", u(self.incarnation)),
            ("basis", self.basis.to_value()),
            ("invalidation", self.invalidation.to_value()),
        ])
    }
}

/// `txnrec = itemcommit / pendingxfer / xferdone / xferabort
///         / xferreopen` — the closed union (D-140; the
/// ImportCommitted marker is WITHDRAWN, D-198).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Txnrec {
    ItemCommit(Itemcommit),
    PendingXfer(Pendingxfer),
    XferDone(Xferdone),
    XferAbort(Xferabort),
    XferReopen(Xferreopen),
}

impl ToValue for Txnrec {
    fn to_value(&self) -> Value {
        match self {
            Txnrec::ItemCommit(r) => r.to_value(),
            Txnrec::PendingXfer(r) => r.to_value(),
            Txnrec::XferDone(r) => r.to_value(),
            Txnrec::XferAbort(r) => r.to_value(),
            Txnrec::XferReopen(r) => r.to_value(),
        }
    }
}

/// `txn = { records: [+ txnrec] }` — ≤ 16 (E8); ONE frame = one
/// atomic multi-record commit; journal order INSIDE one Txn = the
/// records array index (D-200): sequential validation,
/// all-or-nothing — records preserve caller order (NOT a set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Txn {
    pub records: Vec<Txnrec>,
}

impl ToValue for Txn {
    fn to_value(&self) -> Value {
        assert!(
            !self.records.is_empty() && self.records.len() <= 16,
            "txn.records: 1..=16 (E8)"
        );
        cbor::map(vec![(
            "records",
            Value::Array(self.records.iter().map(|r| r.to_value()).collect()),
        )])
    }
}

// ---- signed receipts and leases (§4.7 / A.2) ----

/// `receiptstmt = storagercpt / acceptrcpt / replicaack / ckptwitness`
/// — four closed variants, all bound to plane and zone, all carrying
/// the D-87 feed chain (`prev_stmt`; all-zero at issuer_seq 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Receiptstmt {
    Storage {
        issuer: Issuerid,
        plane_id: Bytes32,
        zone_id: Bytes16,
        subject: Bytes32,
        size: u64,
        seen_ms: u64,
        issuer_seq: u64,
        prev_stmt: Bytes32,
    },
    Accept {
        issuer: Issuerid,
        plane_id: Bytes32,
        zone_id: Bytes16,
        subject: Bytes32,
        seen_ms: u64,
        issuer_seq: u64,
        prev_stmt: Bytes32,
    },
    Replica {
        issuer: Issuerid,
        plane_id: Bytes32,
        zone_id: Bytes16,
        frontier_hash: Bytes32,
        seen_ms: u64,
        issuer_seq: u64,
        prev_stmt: Bytes32,
    },
    Witness {
        issuer: Issuerid,
        plane_id: Bytes32,
        zone_id: Bytes16,
        checkpoint: Bytes32,
        seen_ms: u64,
        issuer_seq: u64,
        prev_stmt: Bytes32,
    },
}

impl ToValue for Receiptstmt {
    fn to_value(&self) -> Value {
        match self {
            Receiptstmt::Storage {
                issuer,
                plane_id,
                zone_id,
                subject,
                size,
                seen_ms,
                issuer_seq,
                prev_stmt,
            } => cbor::map(vec![
                ("v", u(1)),
                ("kind", text("storage")),
                ("issuer", issuer.to_value()),
                ("plane_id", bytes(plane_id)),
                ("zone_id", bytes(zone_id)),
                ("subject", bytes(subject)),
                ("size", u(*size)),
                ("seen_ms", u(*seen_ms)),
                ("issuer_seq", u(*issuer_seq)),
                ("prev_stmt", bytes(prev_stmt)),
            ]),
            Receiptstmt::Accept {
                issuer,
                plane_id,
                zone_id,
                subject,
                seen_ms,
                issuer_seq,
                prev_stmt,
            } => cbor::map(vec![
                ("v", u(1)),
                ("kind", text("accept")),
                ("issuer", issuer.to_value()),
                ("plane_id", bytes(plane_id)),
                ("zone_id", bytes(zone_id)),
                ("subject", bytes(subject)),
                ("seen_ms", u(*seen_ms)),
                ("issuer_seq", u(*issuer_seq)),
                ("prev_stmt", bytes(prev_stmt)),
            ]),
            Receiptstmt::Replica {
                issuer,
                plane_id,
                zone_id,
                frontier_hash,
                seen_ms,
                issuer_seq,
                prev_stmt,
            } => cbor::map(vec![
                ("v", u(1)),
                ("kind", text("replica")),
                ("issuer", issuer.to_value()),
                ("plane_id", bytes(plane_id)),
                ("zone_id", bytes(zone_id)),
                ("frontier_hash", bytes(frontier_hash)),
                ("seen_ms", u(*seen_ms)),
                ("issuer_seq", u(*issuer_seq)),
                ("prev_stmt", bytes(prev_stmt)),
            ]),
            Receiptstmt::Witness {
                issuer,
                plane_id,
                zone_id,
                checkpoint,
                seen_ms,
                issuer_seq,
                prev_stmt,
            } => cbor::map(vec![
                ("v", u(1)),
                ("kind", text("witness")),
                ("issuer", issuer.to_value()),
                ("plane_id", bytes(plane_id)),
                ("zone_id", bytes(zone_id)),
                ("checkpoint", bytes(checkpoint)),
                ("seen_ms", u(*seen_ms)),
                ("issuer_seq", u(*issuer_seq)),
                ("prev_stmt", bytes(prev_stmt)),
            ]),
        }
    }
}

/// `leasestmt = { v: 1, issuer: issuerid, plane_id: bytes32,
///   zone_id: ulid, grant_id: bytes16, lineage: bytes16,
///   issued_ms: ms, expires_ms: ms, ctrl_frontier: bytes32,
///   issuer_seq: uint, prev_stmt: bytes32 }` — ctrl_frontier is
/// diagnostic (§4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leasestmt {
    pub issuer: Issuerid,
    pub plane_id: Bytes32,
    pub zone_id: Bytes16,
    pub grant_id: Bytes16,
    pub lineage: Bytes16,
    pub issued_ms: u64,
    pub expires_ms: u64,
    pub ctrl_frontier: Bytes32,
    pub issuer_seq: u64,
    pub prev_stmt: Bytes32,
}

impl ToValue for Leasestmt {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("v", u(1)),
            ("issuer", self.issuer.to_value()),
            ("plane_id", bytes(&self.plane_id)),
            ("zone_id", bytes(&self.zone_id)),
            ("grant_id", bytes(&self.grant_id)),
            ("lineage", bytes(&self.lineage)),
            ("issued_ms", u(self.issued_ms)),
            ("expires_ms", u(self.expires_ms)),
            ("ctrl_frontier", bytes(&self.ctrl_frontier)),
            ("issuer_seq", u(self.issuer_seq)),
            ("prev_stmt", bytes(&self.prev_stmt)),
        ])
    }
}

/// `Signed<T, tag> = { stmt: T, sig: bstr }` — sig over
/// `msg(tag, stmt)` (§2.3);
/// `stmt_id = H_stmtid(complete Signed bytes)` (D-87).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signedstmt {
    pub stmt: Value,
    pub sig: Vec<u8>,
}

impl ToValue for Signedstmt {
    fn to_value(&self) -> Value {
        cbor::map(vec![("stmt", self.stmt.clone()), ("sig", bytes(&self.sig))])
    }
}

impl Signedstmt {
    pub fn encode(&self) -> Vec<u8> {
        cbor::encode(&self.to_value()).expect("signed statement encodes")
    }

    /// `stmt_id = H_stmtid(complete Signed statement bytes)`.
    pub fn stmt_id(&self) -> Bytes32 {
        h_tag(Tag::Stmtid, &self.encode())
    }
}

fn sign_stmt(stmt: Value, tag: Tag, signer: &OpSigner) -> Signedstmt {
    let payload = cbor::encode(&stmt).expect("statement encodes");
    let sig = match signer {
        OpSigner::Ed25519(sk) => suite::ed25519::sign(sk, tag, &payload).to_vec(),
        OpSigner::P256(sk) => suite::ecdsa_p256::sign(sk, tag, &payload).to_vec(),
    };
    Signedstmt { stmt, sig }
}

/// `Signed<ReceiptStmt, "receipt">` (§2.3 — receipts are signed
/// objects; v0.2 shipped them unsigned, void).
pub fn sign_receipt(stmt: &Receiptstmt, signer: &OpSigner) -> Signedstmt {
    sign_stmt(stmt.to_value(), Tag::Receipt, signer)
}

/// `Signed<LeaseStmt, "lease">`.
pub fn sign_lease(stmt: &Leasestmt, signer: &OpSigner) -> Signedstmt {
    sign_stmt(stmt.to_value(), Tag::Lease, signer)
}

/// `receiptframe = { body: signedreceipt / signedlease }` — 0x16.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receiptframe {
    pub body: Signedstmt,
}

impl ToValue for Receiptframe {
    fn to_value(&self) -> Value {
        cbor::map(vec![("body", self.body.to_value())])
    }
}

#[cfg(test)]
pub(crate) const CDDL_PINS_JOURNAL: &[&str] = &[
    r#"frontier = { v: 1, zone_id: ulid, heads: [* head] }"#,
    r#"ctrlopframe = { op: bstr }        ; 0x01 (control log only): exact
                                  ;   SignedOperation triple bytes
itemrewrapframe = { wrap: itemwrap }                        ; 0x12
fenceframe = { kek_epoch: uint, rotation_op: bytes32,
  fence_frontier: bytes32, control_frontier: bytes32,
  recipients_hash: bytes32 }      ; 0x13 (D-77/D-97)
recipientset = { v: 1,
  pairs: [* { device: bytes16, kem_key: bytes32 }] }"#,
    r#"receiptframe = { body: signedreceipt / signedlease }        ; 0x16
itemcore = { v: 1, aead: "a256gcm", nonce: bstr .size 12, ct: bstr }
itemwrap = { v: 1, item_addr: bytes32, key_wrap_epoch: uint,
  wrapped_dek: bstr .size 48 }   ; AES-256-GCM(32-byte DEK): ct || tag
tombstone = { v: 1, item_addr: bytes32, erase_op: bytes32,
  target_op: bytes32, retired_epoch: uint }
itemcommit = { core: itemcore, wrap: itemwrap, lineage: bytes16,
  gen: uint, seq: uint }
rewrapdone = { kek_epoch: uint, rotation_op: bytes32, count: uint,
  fence_frontier: bytes32,   ; zone Frontier hash at Fence (D-67)
  control_frontier: bytes32, recipients_hash: bytes32,
                             ; equal to the Fence's fields (D-97)
  survivors: bytes32 }"#,
    r#"survivorset = { v: 1, pairs: [* survivorpair] }"#,
    r#"survivorpair = { item_addr: bytes32, wrap_hash: bytes32 }
outboxmark = { through: { lineage: bytes16, gen: uint, seq: uint } }
txnrec = itemcommit / pendingxfer / xferdone / xferabort
       / xferreopen"#,
    r#"txn = { records: [+ txnrec] }         ; <= 16 (E8)
pendingxfer = { export_id: bytes16, release_op: bytes32,
  dest_zone: ulid,
  content_digest: bytes32, record_count: uint }"#,
    r#"xferdone = { export_id: bytes16, release_op: bytes32,
  incarnation: uint,         ; D-192: self-describing interval
  completed: [* bytes32] }"#,
    r#"xferabort = { export_id: bytes16, release_op: bytes32,
  reason: "source-erased" / "reject-permanent"
        / "release-rejected",
  incarnation: uint,
  missing: [+ { rec: bytes32, ? basis: opfactref }] }"#,
    r#"xferreopen = { export_id: bytes16, release_op: bytes32,
  incarnation: uint,
  basis: opfactref, invalidation: factref }"#,
    r#"kekdestroyed = { epoch: uint }   ; the destroyed (retiring) epoch
                                 ;   = new_epoch − 1 (D-92)"#,
    r#"signedreceipt = { stmt: receiptstmt, sig: bstr }
receiptstmt = storagercpt / acceptrcpt / replicaack / ckptwitness
storagercpt = { v: 1, kind: "storage", issuer: issuerid,
  plane_id: bytes32, zone_id: ulid, subject: bytes32,
  size: uint, seen_ms: ms, issuer_seq: uint,
  prev_stmt: bytes32 }"#,
    r#"acceptrcpt = { v: 1, kind: "accept", issuer: issuerid,
  plane_id: bytes32, zone_id: ulid, subject: bytes32,
  seen_ms: ms, issuer_seq: uint, prev_stmt: bytes32 }
replicaack = { v: 1, kind: "replica", issuer: issuerid,
  plane_id: bytes32, zone_id: ulid, frontier_hash: bytes32,
  seen_ms: ms, issuer_seq: uint, prev_stmt: bytes32 }
ckptwitness = { v: 1, kind: "witness", issuer: issuerid,
  plane_id: bytes32, zone_id: ulid,
  checkpoint: bytes32,   ; the c.checkpoint op hash (D-80)
  seen_ms: ms, issuer_seq: uint, prev_stmt: bytes32 }
signedlease = { stmt: leasestmt, sig: bstr }
leasestmt = { v: 1, issuer: issuerid, plane_id: bytes32,
  zone_id: ulid, grant_id: bytes16, lineage: bytes16,
  issued_ms: ms, expires_ms: ms,
  ctrl_frontier: bytes32,   ; diagnostic (S4.7)
  issuer_seq: uint, prev_stmt: bytes32 }"#,
];

#[cfg(test)]
mod tests {
    use super::super::{assert_pins, map_keys};
    use super::*;

    #[test]
    fn cddl_pins_are_verbatim() {
        assert_pins(CDDL_PINS_JOURNAL);
    }

    #[test]
    fn frontier_sorts_by_lineage_then_gen_and_hashes() {
        let h = |lin: u8, gen: u64| Head {
            lineage: [lin; 16],
            gen,
            seq: 1,
            op: [0; 32],
        };
        let f = Frontier {
            zone_id: [1; 16],
            heads: vec![h(2, 1), h(1, 2), h(1, 1)],
        };
        let Value::Map(entries) = f.to_value() else {
            panic!()
        };
        let heads = &entries
            .iter()
            .find(|(k, _)| *k == Value::Text("heads".into()))
            .unwrap()
            .1;
        let Value::Array(items) = heads else { panic!() };
        let order: Vec<(u8, u64)> = items
            .iter()
            .map(|m| {
                let Value::Map(fields) = m else { panic!() };
                let lin = fields
                    .iter()
                    .find_map(|(k, v)| {
                        (*k == Value::Text("lineage".into())).then(|| match v {
                            Value::Bytes(b) => b[0],
                            _ => panic!(),
                        })
                    })
                    .unwrap();
                let gen = fields
                    .iter()
                    .find_map(|(k, v)| {
                        (*k == Value::Text("gen".into())).then(|| match v {
                            Value::Uint(g) => *g,
                            _ => panic!(),
                        })
                    })
                    .unwrap();
                (lin, gen)
            })
            .collect();
        assert_eq!(order, [(1, 1), (1, 2), (2, 1)]);
        // Identity = H_frontier(canonical bytes); zone_id is inside.
        let mut f2 = f.clone();
        f2.zone_id = [9; 16];
        assert_ne!(f.hash(), f2.hash());
        // Duplicate (lineage, gen) is non-canonical.
        let dup = Frontier {
            zone_id: [1; 16],
            heads: vec![h(1, 1), h(1, 1)],
        };
        assert!(std::panic::catch_unwind(move || dup.to_value()).is_err());
    }

    #[test]
    fn frame_bodies_field_sets() {
        let core = Itemcore {
            nonce: [1; 12],
            ct: vec![2; 40],
        };
        assert_eq!(map_keys(&core.to_value()), ["v", "aead", "nonce", "ct"]);
        let wrap = Itemwrap {
            item_addr: [3; 32],
            key_wrap_epoch: 1,
            wrapped_dek: [4; 48],
        };
        assert_eq!(
            map_keys(&wrap.to_value()),
            ["v", "item_addr", "key_wrap_epoch", "wrapped_dek"]
        );
        let commit = Itemcommit {
            core,
            wrap,
            lineage: [5; 16],
            gen: 1,
            seq: 1,
        };
        assert_eq!(
            map_keys(&commit.to_value()),
            ["core", "wrap", "lineage", "gen", "seq"]
        );
        let ts = Tombstone {
            item_addr: [1; 32],
            erase_op: [2; 32],
            target_op: [3; 32],
            retired_epoch: 2,
        };
        assert_eq!(
            map_keys(&ts.to_value()),
            ["v", "item_addr", "erase_op", "target_op", "retired_epoch"]
        );
        assert_eq!(
            map_keys(
                &Fenceframe {
                    kek_epoch: 2,
                    rotation_op: [1; 32],
                    fence_frontier: [2; 32],
                    control_frontier: [3; 32],
                    recipients_hash: [4; 32],
                }
                .to_value()
            ),
            [
                "kek_epoch",
                "rotation_op",
                "fence_frontier",
                "control_frontier",
                "recipients_hash"
            ]
        );
        assert_eq!(
            map_keys(
                &Outboxmark {
                    lineage: [1; 16],
                    gen: 1,
                    seq: 5
                }
                .to_value()
            ),
            ["through"]
        );
    }

    #[test]
    fn recipient_and_survivor_sets_sort_and_hash() {
        let rs = Recipientset {
            pairs: vec![([9; 16], [1; 32]), ([2; 16], [3; 32])],
        };
        let Value::Map(entries) = rs.to_value() else {
            panic!()
        };
        let pairs = &entries
            .iter()
            .find(|(k, _)| *k == Value::Text("pairs".into()))
            .unwrap()
            .1;
        let Value::Array(items) = pairs else { panic!() };
        let first = map_keys(&items[0]);
        assert_eq!(first, ["device", "kem_key"]);
        let Value::Map(fields) = &items[0] else {
            panic!()
        };
        assert!(fields.contains(&(Value::Text("device".into()), Value::Bytes(vec![2; 16]))));
        // Hashes are domain-separated: same pair bytes, different tags.
        let ss = Survivorset {
            pairs: vec![([1; 32], [2; 32])],
        };
        assert_ne!(rs.hash(), ss.hash());
    }

    #[test]
    fn txn_union_and_caps() {
        let px = Pendingxfer {
            export_id: [1; 16],
            release_op: [2; 32],
            dest_zone: [3; 16],
            content_digest: [4; 32],
            record_count: 2,
        };
        assert_eq!(
            map_keys(&px.to_value()),
            [
                "export_id",
                "release_op",
                "dest_zone",
                "content_digest",
                "record_count"
            ]
        );
        let txn = Txn {
            records: vec![Txnrec::PendingXfer(px)],
        };
        assert_eq!(map_keys(&txn.to_value()), ["records"]);
        // Order inside one Txn is the array index (D-200) — NOT sorted.
        let done = Xferdone {
            export_id: [1; 16],
            release_op: [2; 32],
            incarnation: 0,
            completed: vec![[9; 32], [1; 32]],
        };
        let two = Txn {
            records: vec![Txnrec::XferDone(done.clone()), Txnrec::PendingXfer(px)],
        };
        let Value::Map(entries) = two.to_value() else {
            panic!()
        };
        let Value::Array(recs) = &entries[0].1 else {
            panic!()
        };
        assert_eq!(map_keys(&recs[0])[0], "export_id");
        assert!(map_keys(&recs[0]).contains(&"completed".to_string()));
        // completed itself IS a sorted set.
        let Value::Map(df) = &recs[0] else { panic!() };
        let Value::Array(completed) = &df
            .iter()
            .find(|(k, _)| *k == Value::Text("completed".into()))
            .unwrap()
            .1
        else {
            panic!()
        };
        assert_eq!(completed[0], Value::Bytes(vec![1; 32]));
        // Empty and 17-record Txns panic.
        let empty = Txn { records: vec![] };
        assert!(std::panic::catch_unwind(move || empty.to_value()).is_err());
        let too_many = Txn {
            records: (0..17).map(|_| Txnrec::PendingXfer(px)).collect(),
        };
        assert!(std::panic::catch_unwind(move || too_many.to_value()).is_err());
    }

    #[test]
    fn abort_missing_rules() {
        let ab = Xferabort {
            export_id: [1; 16],
            release_op: [2; 32],
            reason: AbortReason::RejectPermanent,
            incarnation: 0,
            missing: vec![
                MissingRec {
                    rec: [9; 32],
                    basis: Some(Opfactref([3; 32])),
                },
                MissingRec {
                    rec: [1; 32],
                    basis: None,
                },
            ],
        };
        let Value::Map(entries) = ab.to_value() else {
            panic!()
        };
        let Value::Array(missing) = &entries
            .iter()
            .find(|(k, _)| *k == Value::Text("missing".into()))
            .unwrap()
            .1
        else {
            panic!()
        };
        // Sorted by rec; the basis-free entry first here.
        assert_eq!(map_keys(&missing[0]), ["rec"]);
        assert_eq!(map_keys(&missing[1]), ["rec", "basis"]);
        let empty = Xferabort {
            missing: vec![],
            ..ab
        };
        assert!(std::panic::catch_unwind(move || empty.to_value()).is_err());
    }

    #[test]
    fn signed_statements_and_stmt_id() {
        let (sk, pk) = crate::suite::ed25519::keypair(&[3u8; 32]);
        let stmt = Receiptstmt::Storage {
            issuer: Issuerid::Device { cert: [1; 32] },
            plane_id: [2; 32],
            zone_id: [3; 16],
            subject: [4; 32],
            size: 100,
            seen_ms: 1000,
            issuer_seq: 1,
            prev_stmt: [0; 32],
        };
        let signed = sign_receipt(&stmt, &OpSigner::Ed25519(&sk));
        assert_eq!(map_keys(&signed.to_value()), ["stmt", "sig"]);
        // The signature is over msg("receipt", stmt bytes).
        let payload = cbor::encode(&stmt.to_value()).unwrap();
        assert!(crate::suite::ed25519::verify(
            &pk,
            Tag::Receipt,
            &payload,
            &signed.sig.as_slice().try_into().unwrap()
        ));
        // A lease over the same bytes signs under a DIFFERENT domain.
        assert!(!crate::suite::ed25519::verify(
            &pk,
            Tag::Lease,
            &payload,
            &signed.sig.as_slice().try_into().unwrap()
        ));
        // stmt_id = H_stmtid(complete Signed bytes) — sig included.
        let id = signed.stmt_id();
        assert_eq!(id, h_tag(Tag::Stmtid, &signed.encode()));
        let lease = Leasestmt {
            issuer: Issuerid::Service { key_id: [5; 32] },
            plane_id: [2; 32],
            zone_id: [3; 16],
            grant_id: [6; 16],
            lineage: [7; 16],
            issued_ms: 1,
            expires_ms: 2,
            ctrl_frontier: [8; 32],
            issuer_seq: 1,
            prev_stmt: [0; 32],
        };
        assert_eq!(
            map_keys(&lease.to_value()),
            [
                "v",
                "issuer",
                "plane_id",
                "zone_id",
                "grant_id",
                "lineage",
                "issued_ms",
                "expires_ms",
                "ctrl_frontier",
                "issuer_seq",
                "prev_stmt"
            ]
        );
        // Receipt variants carry their kind literals.
        for (stmt, kind) in [
            (
                Receiptstmt::Accept {
                    issuer: Issuerid::Device { cert: [1; 32] },
                    plane_id: [2; 32],
                    zone_id: [3; 16],
                    subject: [4; 32],
                    seen_ms: 1,
                    issuer_seq: 1,
                    prev_stmt: [0; 32],
                },
                "accept",
            ),
            (
                Receiptstmt::Replica {
                    issuer: Issuerid::Device { cert: [1; 32] },
                    plane_id: [2; 32],
                    zone_id: [3; 16],
                    frontier_hash: [4; 32],
                    seen_ms: 1,
                    issuer_seq: 1,
                    prev_stmt: [0; 32],
                },
                "replica",
            ),
            (
                Receiptstmt::Witness {
                    issuer: Issuerid::Device { cert: [1; 32] },
                    plane_id: [2; 32],
                    zone_id: [3; 16],
                    checkpoint: [4; 32],
                    seen_ms: 1,
                    issuer_seq: 1,
                    prev_stmt: [0; 32],
                },
                "witness",
            ),
        ] {
            let Value::Map(fields) = stmt.to_value() else {
                panic!()
            };
            assert!(fields.contains(&(Value::Text("kind".into()), Value::Text(kind.into()))));
        }
    }
}
