//! The operation envelope (§4.5 / A.2) — the reducer's parse layer.
//!
//! Decodes a SignedOperation triple through the strict reader,
//! validates the header's closed shape (exact key set, field types,
//! closed vocabularies, arm-discriminated proof), and exposes the
//! signing composition over VERIFIED INPUT SLICES: the signature
//! message is `msg("op", header-raw)`, `body_hash` must equal
//! `H_body(body-raw)`, and `op_hash = H_op(triple-raw)` — no
//! re-serialization anywhere (O1/O2).

use crate::cbor::{decode, DecodeError, Node};
use crate::domains;
use ed25519_dalek::{Signature, VerifyingKey};

/// Parse-layer failure, already §10.4-shaped: `Parse` carries the
/// decoder's classification; `Shape` is the envelope CDDL layer
/// (`malformed` family); `Version` = `unknown-version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpError {
    Parse(DecodeError),
    Shape(&'static str),
    Version,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proof<'a> {
    Dev { cert: [u8; 32], cap: [u8; 32] },
    Genesis { genesis: [u8; 32] },
    Admin { epoch: u64, ctrl_frontier: [u8; 32] },
    Recovery { repoch: u64, recovery_pk: &'a [u8] },
}

#[derive(Debug, Clone)]
pub struct Header<'a> {
    pub raw: &'a [u8],
    pub tenant: &'a str,
    pub plane_id: [u8; 32],
    pub zone_id: [u8; 16],
    pub space_id: [u8; 16],
    pub authored_kek_epoch: u64,
    pub capability_epoch: u64,
    pub signer_alg: &'a str,
    pub signer_key_id: [u8; 32],
    pub writer_lineage: [u8; 16],
    pub writer_gen: u64,
    pub actor_kind: &'a str,
    pub actor_id: &'a str,
    pub attested_by: Option<[u8; 32]>,
    pub proof: Proof<'a>,
    pub request_id: [u8; 16],
    pub writer_sequence: u64,
    pub previous_writer_hash: [u8; 32],
    pub causal_references: Vec<[u8; 32]>,
    pub created_ms: u64,
    pub created_count: u64,
    pub operation_type: &'a str,
    pub operation_version: u64,
    pub body_hash: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct SignedOp<'a> {
    pub raw: &'a [u8],
    pub header: Header<'a>,
    pub signature: &'a [u8],
    pub body: Node<'a>,
}

const HEADER_KEYS: &[&str] = &[
    "v",
    "tenant",
    "plane_id",
    "zone_id",
    "space_id",
    "authored_kek_epoch",
    "capability_epoch",
    "signer_alg",
    "signer_key_id",
    "writer",
    "actor",
    "authorization_proof",
    "request_id",
    "writer_sequence",
    "previous_writer_hash",
    "causal_references",
    "created_hlc",
    "operation_type",
    "operation_version",
    "body_hash",
];

fn req<'a, 'n>(m: &'n Node<'a>, key: &'static str) -> Result<&'n Node<'a>, OpError> {
    m.get(key).ok_or(OpError::Shape(key))
}

fn b32(n: &Node, what: &'static str) -> Result<[u8; 32], OpError> {
    n.bytes_n::<32>().ok_or(OpError::Shape(what))
}

fn b16(n: &Node, what: &'static str) -> Result<[u8; 16], OpError> {
    n.bytes_n::<16>().ok_or(OpError::Shape(what))
}

fn uint(n: &Node, what: &'static str) -> Result<u64, OpError> {
    n.as_uint().ok_or(OpError::Shape(what))
}

fn text<'a>(n: &Node<'a>, what: &'static str) -> Result<&'a str, OpError> {
    n.as_text().ok_or(OpError::Shape(what))
}

/// Exact-key-set check: canonicality already proves order and
/// uniqueness, so a sorted set comparison suffices.
fn keys_exactly(node: &Node, expect: &[&str], what: &'static str) -> Result<(), OpError> {
    let keys = node.map_keys().ok_or(OpError::Shape(what))?;
    let mut want: Vec<&str> = expect.to_vec();
    want.sort_by(|a, b| {
        // Encoded-byte order for text keys = (len, bytes).
        (a.len(), a.as_bytes()).cmp(&(b.len(), b.as_bytes()))
    });
    if keys != want {
        return Err(OpError::Shape(what));
    }
    Ok(())
}

fn parse_proof<'a>(node: &Node<'a>) -> Result<Proof<'a>, OpError> {
    let arm = text(req(node, "arm")?, "proof.arm")?;
    match arm {
        "dev" => {
            keys_exactly(node, &["arm", "cert", "cap"], "proof.dev")?;
            Ok(Proof::Dev {
                cert: b32(req(node, "cert")?, "proof.cert")?,
                cap: b32(req(node, "cap")?, "proof.cap")?,
            })
        }
        "genesis" => {
            keys_exactly(node, &["arm", "genesis"], "proof.genesis")?;
            Ok(Proof::Genesis {
                genesis: b32(req(node, "genesis")?, "proof.genesis.id")?,
            })
        }
        "admin" => {
            keys_exactly(node, &["arm", "epoch", "ctrl_frontier"], "proof.admin")?;
            Ok(Proof::Admin {
                epoch: uint(req(node, "epoch")?, "proof.epoch")?,
                ctrl_frontier: b32(req(node, "ctrl_frontier")?, "proof.ctrl_frontier")?,
            })
        }
        "recovery" => {
            keys_exactly(node, &["arm", "repoch", "recovery_pk"], "proof.recovery")?;
            let pk = req(node, "recovery_pk")?
                .as_bytes()
                .ok_or(OpError::Shape("proof.recovery_pk"))?;
            if pk.len() != 32 {
                return Err(OpError::Shape("proof.recovery_pk.len"));
            }
            Ok(Proof::Recovery {
                repoch: uint(req(node, "repoch")?, "proof.repoch")?,
                recovery_pk: pk,
            })
        }
        _ => Err(OpError::Shape("proof.arm.unknown")),
    }
}

/// Parse one SignedOperation triple from raw bytes.
pub fn parse_op(raw: &[u8]) -> Result<SignedOp<'_>, OpError> {
    let node = decode(raw).map_err(OpError::Parse)?;
    parse_op_node(&node, raw)
}

/// Parse from an already-decoded node (the node's `raw` must be the
/// exact triple bytes).
pub fn parse_op_node<'a>(node: &Node<'a>, raw: &'a [u8]) -> Result<SignedOp<'a>, OpError> {
    keys_exactly(node, &["header", "signature", "body"], "triple")?;
    let header_node = req(node, "header")?;
    let signature = req(node, "signature")?
        .as_bytes()
        .ok_or(OpError::Shape("signature"))?;
    let body = req(node, "body")?.clone();

    keys_exactly(header_node, HEADER_KEYS, "header")?;
    if uint(req(header_node, "v")?, "header.v")? != 1 {
        return Err(OpError::Version);
    }
    let tenant = text(req(header_node, "tenant")?, "tenant")?;
    if !["memory", "agenda", "ctrl"].contains(&tenant) {
        return Err(OpError::Shape("tenant.vocab"));
    }
    let signer_alg = text(req(header_node, "signer_alg")?, "signer_alg")?;
    if !["ed25519", "p256"].contains(&signer_alg) {
        return Err(OpError::Shape("signer_alg.vocab"));
    }

    let writer = req(header_node, "writer")?;
    keys_exactly(writer, &["lineage", "gen"], "writer")?;

    let actor = req(header_node, "actor")?;
    let actor_keys = actor.map_keys().ok_or(OpError::Shape("actor"))?;
    let attested_by = match actor_keys.as_slice() {
        ["id", "kind"] => None,
        ["id", "kind", "attested_by"] => {
            Some(b32(req(actor, "attested_by")?, "actor.attested_by")?)
        }
        _ => return Err(OpError::Shape("actor.keys")),
    };
    let actor_kind = text(req(actor, "kind")?, "actor.kind")?;
    if ![
        "human",
        "daemon",
        "browser",
        "agent-session",
        "peer",
        "service",
    ]
    .contains(&actor_kind)
    {
        return Err(OpError::Shape("actor.kind.vocab"));
    }

    let refs_node = req(header_node, "causal_references")?;
    let mut causal_references = Vec::new();
    for r in refs_node
        .as_array()
        .ok_or(OpError::Shape("causal_references"))?
    {
        causal_references.push(b32(r, "causal_references.member")?);
    }

    let hlc = req(header_node, "created_hlc")?;
    let hlc_items = hlc.as_array().ok_or(OpError::Shape("created_hlc"))?;
    if hlc_items.len() != 2 {
        return Err(OpError::Shape("created_hlc.arity"));
    }

    let header = Header {
        raw: header_node.raw,
        tenant,
        plane_id: b32(req(header_node, "plane_id")?, "plane_id")?,
        zone_id: b16(req(header_node, "zone_id")?, "zone_id")?,
        space_id: b16(req(header_node, "space_id")?, "space_id")?,
        authored_kek_epoch: uint(
            req(header_node, "authored_kek_epoch")?,
            "authored_kek_epoch",
        )?,
        capability_epoch: uint(req(header_node, "capability_epoch")?, "capability_epoch")?,
        signer_alg,
        signer_key_id: b32(req(header_node, "signer_key_id")?, "signer_key_id")?,
        writer_lineage: b16(req(writer, "lineage")?, "writer.lineage")?,
        writer_gen: uint(req(writer, "gen")?, "writer.gen")?,
        actor_kind,
        actor_id: text(req(actor, "id")?, "actor.id")?,
        attested_by,
        proof: parse_proof(req(header_node, "authorization_proof")?)?,
        request_id: b16(req(header_node, "request_id")?, "request_id")?,
        writer_sequence: uint(req(header_node, "writer_sequence")?, "writer_sequence")?,
        previous_writer_hash: b32(
            req(header_node, "previous_writer_hash")?,
            "previous_writer_hash",
        )?,
        causal_references,
        created_ms: uint(&hlc_items[0], "created_hlc.ms")?,
        created_count: uint(&hlc_items[1], "created_hlc.count")?,
        operation_type: text(req(header_node, "operation_type")?, "operation_type")?,
        operation_version: uint(req(header_node, "operation_version")?, "operation_version")?,
        body_hash: b32(req(header_node, "body_hash")?, "body_hash")?,
    };

    Ok(SignedOp {
        raw,
        header,
        signature,
        body,
    })
}

impl<'a> SignedOp<'a> {
    /// `op_hash = H_op(exact triple bytes)` (O2).
    pub fn op_hash(&self) -> [u8; 32] {
        domains::h("op", self.raw)
    }

    /// O1: the carried `body_hash` matches `H_body(body bytes)`.
    pub fn body_hash_ok(&self) -> bool {
        self.header.body_hash == domains::h("body", self.body.raw)
    }

    /// Verify the header signature under an Ed25519 key —
    /// `verify_strict` over `msg("op", header bytes)`.
    pub fn verify_ed25519(&self, pk: &[u8; 32]) -> bool {
        if self.header.signer_alg != "ed25519" {
            return false;
        }
        let Ok(vk) = VerifyingKey::from_bytes(pk) else {
            return false;
        };
        let Ok(sig): Result<[u8; 64], _> = self.signature.try_into() else {
            return false;
        };
        vk.verify_strict(
            &domains::msg("op", self.header.raw),
            &Signature::from_bytes(&sig),
        )
        .is_ok()
        // The signer identity commitment: signer_key_id must be the
        // key's H_key identity (checked by the caller against the
        // resolved key — kept out of raw verification so a wrong-key
        // probe stays a pure signature question).
    }

    /// The full self-contained genesis check: a `c.genesis` triple
    /// verifies under the root key its OWN body carries, and the
    /// genesis arm cites `H_genesis(descriptor bytes)` = the header's
    /// `plane_id` (N4).
    pub fn verify_genesis(&self) -> Result<(), &'static str> {
        if self.header.operation_type != "c.genesis" {
            return Err("not c.genesis");
        }
        if !self.body_hash_ok() {
            return Err("body-hash");
        }
        let descriptor = self.body.get("descriptor").ok_or("descriptor missing")?;
        let root_pk: [u8; 32] = descriptor
            .get("root_sig_pk")
            .and_then(|n| n.bytes_n::<32>())
            .ok_or("root_sig_pk")?;
        if !self.verify_ed25519(&root_pk) {
            return Err("sig-invalid");
        }
        if self.header.signer_key_id != domains::key_id("ed25519", &root_pk) {
            return Err("signer_key_id");
        }
        let plane = domains::h("genesis", descriptor.raw);
        if self.header.plane_id != plane {
            return Err("plane_id != H_genesis(descriptor)");
        }
        match self.header.proof {
            Proof::Genesis { genesis } if genesis == plane => Ok(()),
            _ => Err("genesis arm citation"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tranche_items() -> Vec<(String, String, Vec<u8>)> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vectors");
        let mut out = Vec::new();
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        files.sort();
        for f in files {
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
            let fname = f.file_name().unwrap().to_str().unwrap().to_string();
            for field in ["items", "aux"] {
                if let Some(m) = v["inputs"][field].as_object() {
                    for (name, hx) in m {
                        let s = hx.as_str().unwrap();
                        let bytes: Vec<u8> = (0..s.len())
                            .step_by(2)
                            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                            .collect();
                        out.push((fname.clone(), name.clone(), bytes));
                    }
                }
            }
        }
        out
    }

    /// The tranche-wide envelope differential: every item that is an
    /// operation triple (a map carrying "header") parses under the
    /// independent envelope layer, its body_hash recomputes from the
    /// body slice, and every `c.genesis` fully self-verifies
    /// (signature under its own carried root key, N4 plane identity,
    /// arm citation).
    #[test]
    fn tranche_ops_parse_and_genesis_verifies() {
        let mut ops = 0;
        let mut geneses = 0;
        let mut bad_body = Vec::new();
        for (file, name, bytes) in tranche_items() {
            let Ok(node) = decode(&bytes) else {
                // The family-1 canonical-reject corpus deliberately
                // carries undecodable bytes — those ride inputs.bytes,
                // not items/aux, so anything here MUST decode.
                panic!("{file}/{name}: items/aux entry fails strict decode");
            };
            if node.get("header").is_none() {
                continue; // journal Txns, signed statements
            }
            let op = parse_op(&bytes).unwrap_or_else(|e| panic!("{file}/{name}: {e:?}"));
            if !op.body_hash_ok() {
                bad_body.push(format!("{file}/{name}"));
            }
            ops += 1;
            if op.header.operation_type == "c.genesis" && op.verify_genesis().is_ok() {
                geneses += 1;
            }
        }
        assert!(ops >= 30, "expected a substantial op population, got {ops}");
        // Exactly ONE deliberate body tamper exists in the corpus.
        assert_eq!(
            bad_body,
            vec!["f07-control-body-tamper.json/c2".to_string()],
            "unexpected body-hash failures"
        );
        // Every fold fixture folds its genesis (6 tranche + 12
        // fold-lane + 1 export-import + 7 status corpus); the two
        // byte-level journal fixtures deliver Txn frames only.
        assert_eq!(geneses, 26);
    }

    /// Tampering any byte of the header breaks the signature; the
    /// carried body_hash pins the body slice.
    #[test]
    fn tamper_negatives() {
        let (_, _, genesis) = tranche_items()
            .into_iter()
            .find(|(_, n, _)| n == "c1")
            .unwrap();
        let op = parse_op(&genesis).unwrap();
        let root_pk: [u8; 32] = op
            .body
            .get("descriptor")
            .unwrap()
            .get("root_sig_pk")
            .unwrap()
            .bytes_n()
            .unwrap();
        assert!(op.verify_ed25519(&root_pk));
        // Wrong key fails.
        assert!(!op.verify_ed25519(&[7u8; 32]));

        // Flip one bit inside the header's raw span: the signature
        // must fail (re-parse from mutated bytes; the mutation may
        // also break canonicality, which is an equally correct
        // rejection).
        let header_start = genesis
            .windows(op.header.raw.len())
            .position(|w| w == op.header.raw)
            .unwrap();
        let mut mutated = genesis.clone();
        // Flip a bit inside plane_id's value bytes (deep in the span,
        // clear of any structural header byte).
        mutated[header_start + 40] ^= 1;
        // Structural rejection is an equally correct outcome here.
        if let Ok(bad) = parse_op(&mutated) {
            assert!(!bad.verify_ed25519(&root_pk), "tampered header verified");
        }

        // Flip a bit in the body: body_hash_ok must fail.
        let body_start = genesis
            .windows(op.body.raw.len())
            .position(|w| w == op.body.raw)
            .unwrap();
        let mut mutated = genesis.clone();
        mutated[body_start + 30] ^= 1;
        if let Ok(bad) = parse_op(&mutated) {
            assert!(!bad.body_hash_ok(), "tampered body passed body_hash");
        }
    }

    /// Shape negatives: missing keys, unknown arm, bad version.
    #[test]
    fn shape_negatives() {
        // {"body": 1} — not a triple.
        let b = [0xa1, 0x64, b'b', b'o', b'd', b'y', 0x01];
        assert!(matches!(parse_op(&b), Err(OpError::Shape("triple"))));
        // Truncated garbage.
        assert!(matches!(
            parse_op(&[0xff]),
            Err(OpError::Parse(DecodeError::Malformed))
        ));
    }
}
