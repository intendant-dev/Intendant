//! Appendix A.2 — the operation envelope (§4.5) and the signing
//! composition:
//!
//! - `body_hash = H_body(canonical body)` (O1)
//! - the signature covers `msg("op", header)` (O1)
//! - `op_hash = H_op(triple)` — the durable identity (O2); replicas
//!   store exact bytes, never verify-after-reserialize
//!
//! plus the §2 derivation domains `gen_start`, `assert_req`, `mat_id`.

use super::identity::Authproof;
use super::{
    bytes, closed_vocab, sorted_set_default, text, u, Bytes16, Bytes32, Hlc, Sigalg, ToValue,
};
use crate::cbor::{self, Value};
use crate::domains::{h_tag, Tag};
use crate::suite;

closed_vocab!(Tenant {
    Memory => "memory",
    Agenda => "agenda",
    Ctrl => "ctrl",
});

closed_vocab!(ActorKind {
    Human => "human",
    Daemon => "daemon",
    Browser => "browser",
    AgentSession => "agent-session",
    Peer => "peer",
    Service => "service",
});

/// `writer: { lineage: bytes16, gen: uint }`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Writer {
    pub lineage: Bytes16,
    pub gen: u64,
}

impl ToValue for Writer {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("lineage", bytes(&self.lineage)),
            ("gen", u(self.gen)),
        ])
    }
}

/// `actor: { kind: ..., id: text, ? attested_by: bytes32 }` —
/// `actor.id` minting is closed per kind (O8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub kind: ActorKind,
    pub id: String,
    pub attested_by: Option<Bytes32>,
}

impl Actor {
    /// O7: control operations carry `{ kind: "human", id: "owner" }`.
    pub fn owner() -> Actor {
        Actor {
            kind: ActorKind::Human,
            id: "owner".into(),
            attested_by: None,
        }
    }
}

impl ToValue for Actor {
    fn to_value(&self) -> Value {
        let mut entries = vec![("kind", self.kind.to_value()), ("id", text(&self.id))];
        if let Some(a) = &self.attested_by {
            entries.push(("attested_by", bytes(a)));
        }
        cbor::map(entries)
    }
}

/// `header = { v: 1, tenant: "memory" / "agenda" / "ctrl",
///   plane_id: bytes32, zone_id: ulid, space_id: ulid,
///   authored_kek_epoch: uint, capability_epoch: uint,
///   signer_alg: sigalg, signer_key_id: bytes32,
///   writer: { lineage: bytes16, gen: uint }, actor: {...},
///   authorization_proof: authproof, request_id: bytes16,
///   writer_sequence: uint, previous_writer_hash: bytes32,
///   causal_references: [* bytes32], created_hlc: hlc,
///   operation_type: text, operation_version: uint, body_hash: bytes32 }`
///
/// `causal_references` is a set (§4.5) — default E7 key.
/// O7: control operations pin `authored_kek_epoch = 0` and
/// `capability_epoch = 0` (0 is invalid on tenant operations).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub tenant: Tenant,
    pub plane_id: Bytes32,
    pub zone_id: Bytes16,
    pub space_id: Bytes16,
    pub authored_kek_epoch: u64,
    pub capability_epoch: u64,
    pub signer_alg: Sigalg,
    pub signer_key_id: Bytes32,
    pub writer: Writer,
    pub actor: Actor,
    pub authorization_proof: Authproof,
    pub request_id: Bytes16,
    pub writer_sequence: u64,
    pub previous_writer_hash: Bytes32,
    pub causal_references: Vec<Bytes32>,
    pub created_hlc: Hlc,
    pub operation_type: String,
    pub operation_version: u64,
    pub body_hash: Bytes32,
}

impl ToValue for Header {
    fn to_value(&self) -> Value {
        let refs = sorted_set_default(
            self.causal_references.iter().map(|r| bytes(r)).collect(),
            "header.causal_references",
        );
        cbor::map(vec![
            ("v", u(1)),
            ("tenant", self.tenant.to_value()),
            ("plane_id", bytes(&self.plane_id)),
            ("zone_id", bytes(&self.zone_id)),
            ("space_id", bytes(&self.space_id)),
            ("authored_kek_epoch", u(self.authored_kek_epoch)),
            ("capability_epoch", u(self.capability_epoch)),
            ("signer_alg", self.signer_alg.to_value()),
            ("signer_key_id", bytes(&self.signer_key_id)),
            ("writer", self.writer.to_value()),
            ("actor", self.actor.to_value()),
            ("authorization_proof", self.authorization_proof.to_value()),
            ("request_id", bytes(&self.request_id)),
            ("writer_sequence", u(self.writer_sequence)),
            ("previous_writer_hash", bytes(&self.previous_writer_hash)),
            ("causal_references", Value::Array(refs)),
            ("created_hlc", self.created_hlc.to_value()),
            ("operation_type", text(&self.operation_type)),
            ("operation_version", u(self.operation_version)),
            ("body_hash", bytes(&self.body_hash)),
        ])
    }
}

impl Header {
    /// Canonical header bytes — the exact signature message payload.
    pub fn encode(&self) -> Vec<u8> {
        cbor::encode(&self.to_value()).expect("header encodes")
    }
}

/// `body_hash = H_body(body)` over the canonical plaintext body (O1).
pub fn body_hash(body: &Value) -> Bytes32 {
    h_tag(Tag::Body, &cbor::encode(body).expect("body encodes"))
}

/// The signer for `seal_op` — must match `header.signer_alg`.
pub enum OpSigner<'a> {
    Ed25519(&'a ed25519_dalek::SigningKey),
    P256(&'a p256::ecdsa::SigningKey),
}

/// `signedop = { header: header, signature: bstr, body: opbody }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signedop {
    pub header: Header,
    pub signature: Vec<u8>,
    pub body: Value,
}

/// Seal the triple: set `header.body_hash = H_body(body)`, sign
/// `msg("op", header)` with the matching key (O1). Panics on a
/// signer/`signer_alg` mismatch — a minting bug.
pub fn seal_op(mut header: Header, body: Value, signer: &OpSigner) -> Signedop {
    header.body_hash = body_hash(&body);
    let msg_bytes = header.encode();
    let signature: Vec<u8> = match (signer, header.signer_alg) {
        (OpSigner::Ed25519(sk), Sigalg::Ed25519) => {
            suite::ed25519::sign(sk, Tag::Op, &msg_bytes).to_vec()
        }
        (OpSigner::P256(sk), Sigalg::P256) => {
            suite::ecdsa_p256::sign(sk, Tag::Op, &msg_bytes).to_vec()
        }
        _ => panic!("signer does not match header.signer_alg"),
    };
    Signedop {
        header,
        signature,
        body,
    }
}

impl ToValue for Signedop {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("header", self.header.to_value()),
            ("signature", bytes(&self.signature)),
            ("body", self.body.clone()),
        ])
    }
}

impl Signedop {
    /// Canonical triple bytes — what replicas store (O2).
    pub fn encode(&self) -> Vec<u8> {
        cbor::encode(&self.to_value()).expect("triple encodes")
    }

    /// `op_hash = H_op(triple)` — the durable identity (O2).
    pub fn op_hash(&self) -> Bytes32 {
        h_tag(Tag::Op, &self.encode())
    }

    /// Fixture-side sanity: the header signature verifies under `pk`
    /// and `body_hash` matches the carried body.
    pub fn verify(&self, pk_ed25519: &Bytes32) -> bool {
        let Ok(sig): Result<[u8; 64], _> = self.signature.as_slice().try_into() else {
            return false;
        };
        self.header.body_hash == body_hash(&self.body)
            && suite::ed25519::verify(pk_ed25519, Tag::Op, &self.header.encode(), &sig)
    }
}

/// `gen_start(lineage, gen) = H_genstart(lineage || gen_be64)` (§9.3)
/// — the `previous_writer_hash` of a generation's opening operation
/// (O6: generation 1 opens directly; g ≥ 2 opens with `w.gen`).
pub fn gen_start(lineage: &Bytes16, gen: u64) -> Bytes32 {
    let mut preimage = Vec::with_capacity(24);
    preimage.extend_from_slice(lineage);
    preimage.extend_from_slice(&gen.to_be_bytes());
    h_tag(Tag::Genstart, &preimage)
}

/// `assert_req(claim_req_id) = H_assertreq(claim_req_id)[0..16]` (§11.1).
pub fn assert_req(claim_req_id: &Bytes16) -> Bytes16 {
    let full = h_tag(Tag::Assertreq, claim_req_id);
    full[..16].try_into().expect("16-byte prefix")
}

/// `mat_id = H_mat(SEC1 point bytes)` (D-175) — the role-neutral
/// key-material identity.
pub fn mat_id(sec1_point: &[u8]) -> Bytes32 {
    h_tag(Tag::Mat, sec1_point)
}

#[cfg(test)]
pub(crate) const CDDL_PINS_ENVELOPE: &[&str] = &[
    r#"header = { v: 1, tenant: "memory" / "agenda" / "ctrl",
  plane_id: bytes32, zone_id: ulid, space_id: ulid,
  authored_kek_epoch: uint, capability_epoch: uint,
  signer_alg: sigalg, signer_key_id: bytes32,
  writer: { lineage: bytes16, gen: uint },
  actor: { kind: "human" / "daemon" / "browser" / "agent-session"
                / "peer" / "service", id: text, ? attested_by: bytes32 },
  authorization_proof: authproof, request_id: bytes16,
  writer_sequence: uint, previous_writer_hash: bytes32,
  causal_references: [* bytes32], created_hlc: hlc,
  operation_type: text, operation_version: uint, body_hash: bytes32 }
signedop = { header: header, signature: bstr, body: opbody }"#,
    r#"gen_start(lineage, gen)   = H_genstart(lineage || gen_be64)      # §9.3
assert_req(claim_req_id)  = H_assertreq(claim_req_id)[0..16]     # §11.1"#,
    "`mat_id = H_mat(SEC1 point bytes)`",
];

#[cfg(test)]
mod tests {
    use super::super::{assert_pins, map_keys};
    use super::*;
    use crate::domains::msg;

    fn test_header(body_hash: Bytes32) -> Header {
        Header {
            tenant: Tenant::Ctrl,
            plane_id: [1; 32],
            zone_id: [2; 16],
            space_id: [3; 16],
            authored_kek_epoch: 0,
            capability_epoch: 0,
            signer_alg: Sigalg::Ed25519,
            signer_key_id: [4; 32],
            writer: Writer {
                lineage: [5; 16],
                gen: 1,
            },
            actor: Actor::owner(),
            authorization_proof: Authproof::Genesis { genesis: [6; 32] },
            request_id: [7; 16],
            writer_sequence: 1,
            previous_writer_hash: gen_start(&[5; 16], 1),
            causal_references: vec![],
            created_hlc: Hlc { ms: 1000, count: 0 },
            operation_type: "c.genesis".into(),
            operation_version: 1,
            body_hash,
        }
    }

    #[test]
    fn cddl_pins_are_verbatim() {
        assert_pins(CDDL_PINS_ENVELOPE);
    }

    #[test]
    fn header_field_set_and_order() {
        let h = test_header([0; 32]);
        assert_eq!(
            map_keys(&h.to_value()),
            [
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
                "body_hash"
            ]
        );
    }

    #[test]
    fn causal_references_are_a_sorted_set() {
        let mut h = test_header([0; 32]);
        h.causal_references = vec![[9; 32], [1; 32], [5; 32]];
        let Value::Map(entries) = h.to_value() else {
            panic!()
        };
        let refs = &entries
            .iter()
            .find(|(k, _)| *k == Value::Text("causal_references".into()))
            .unwrap()
            .1;
        assert_eq!(
            refs,
            &Value::Array(vec![
                Value::Bytes(vec![1; 32]),
                Value::Bytes(vec![5; 32]),
                Value::Bytes(vec![9; 32]),
            ])
        );
        h.causal_references = vec![[1; 32], [1; 32]];
        assert!(std::panic::catch_unwind(move || h.to_value()).is_err());
    }

    #[test]
    fn seal_op_composition() {
        let (sk, pk) = crate::suite::ed25519::keypair(&[7u8; 32]);
        let body = cbor::map(vec![("nonce", bytes(&[1u8; 16]))]);
        let sealed = seal_op(test_header([0; 32]), body.clone(), &OpSigner::Ed25519(&sk));
        // body_hash was set from the body, not the placeholder.
        assert_eq!(sealed.header.body_hash, body_hash(&body));
        assert_ne!(sealed.header.body_hash, [0; 32]);
        // The signature covers msg("op", header) — verify both through
        // the helper and against the raw framed message.
        assert!(sealed.verify(&pk));
        use ed25519_dalek::Verifier;
        let raw = msg(Tag::Op, &sealed.header.encode());
        let sig =
            ed25519_dalek::Signature::from_bytes(sealed.signature.as_slice().try_into().unwrap());
        assert!(sk.verifying_key().verify(&raw, &sig).is_ok());
        // Triple shape and identity.
        assert_eq!(
            map_keys(&sealed.to_value()),
            ["header", "signature", "body"]
        );
        assert_eq!(sealed.op_hash(), h_tag(Tag::Op, &sealed.encode()));
        // Tampering the body breaks the body_hash check.
        let mut tampered = sealed.clone();
        tampered.body = cbor::map(vec![("nonce", bytes(&[2u8; 16]))]);
        assert!(!tampered.verify(&pk));
    }

    #[test]
    fn seal_op_rejects_alg_mismatch() {
        let (sk, _) = crate::suite::ed25519::keypair(&[7u8; 32]);
        let mut h = test_header([0; 32]);
        h.signer_alg = Sigalg::P256;
        let body = cbor::map(vec![]);
        assert!(
            std::panic::catch_unwind(move || seal_op(h, body, &OpSigner::Ed25519(&sk))).is_err()
        );
    }

    #[test]
    fn p256_sealing_verifies_low_s() {
        let (sk, pk) = crate::suite::ecdsa_p256::keypair(&[9u8; 32]).unwrap();
        let mut h = test_header([0; 32]);
        h.signer_alg = Sigalg::P256;
        let sealed = seal_op(h, cbor::map(vec![]), &OpSigner::P256(&sk));
        assert!(crate::suite::ecdsa_p256::verify(
            &pk,
            Tag::Op,
            &sealed.header.encode(),
            &sealed.signature
        ));
    }

    #[test]
    fn derivation_domains() {
        let lineage = [5u8; 16];
        // gen_start = H_genstart over the 24-byte concatenation.
        let mut preimage = lineage.to_vec();
        preimage.extend_from_slice(&2u64.to_be_bytes());
        assert_eq!(gen_start(&lineage, 2), h_tag(Tag::Genstart, &preimage));
        assert_ne!(gen_start(&lineage, 1), gen_start(&lineage, 2));
        // assert_req = the 16-byte prefix of H_assertreq.
        let req = [3u8; 16];
        assert_eq!(assert_req(&req), h_tag(Tag::Assertreq, &req)[..16]);
        // mat_id is role-neutral: same point, one identity.
        assert_eq!(mat_id(&[4u8; 65]), h_tag(Tag::Mat, &[4u8; 65]));
    }

    #[test]
    fn actor_and_writer_shapes() {
        assert_eq!(map_keys(&Actor::owner().to_value()), ["kind", "id"]);
        let attested = Actor {
            kind: ActorKind::AgentSession,
            id: "sess-1".into(),
            attested_by: Some([8; 32]),
        };
        assert_eq!(
            map_keys(&attested.to_value()),
            ["kind", "id", "attested_by"]
        );
        assert_eq!(
            map_keys(
                &Writer {
                    lineage: [1; 16],
                    gen: 3
                }
                .to_value()
            ),
            ["lineage", "gen"]
        );
        assert_eq!(Tenant::ALL.len(), 3);
        assert_eq!(ActorKind::ALL.len(), 6);
    }
}
