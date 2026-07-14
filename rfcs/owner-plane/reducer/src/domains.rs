//! Hash/signature domains (§2) — the reducer's own implementation.
//!
//! `msg(tag, x) = "intendant/" || tag || "/v1" || 0x00 || x`;
//! `h(tag, x) = SHA-256(msg(tag, x))`. The tag inventory is closed
//! (E10); the drift-gate test pins it set-equal to the companion
//! schema's `hash-domain` contract, independently of the reference
//! core's identical gate.

use sha2::{Digest, Sha256};

/// The closed §2 inventory, wire names exactly.
pub const TAGS: &[&str] = &[
    "genesis",
    "cert",
    "grant",
    "op",
    "body",
    "frontier",
    "receipt",
    "lease",
    "drill",
    "kek",
    "item",
    "policy",
    "key",
    "evrec",
    "genstart",
    "assertreq",
    "reauth",
    "cutoffreq",
    "abandonreq",
    "stmtid",
    "recips",
    "survivors",
    "brec",
    "bnode",
    "mat",
];

pub fn msg(tag: &str, x: &[u8]) -> Vec<u8> {
    debug_assert!(TAGS.contains(&tag), "unknown domain tag: {tag}");
    let mut m = Vec::with_capacity(14 + tag.len() + x.len());
    m.extend_from_slice(b"intendant/");
    m.extend_from_slice(tag.as_bytes());
    m.extend_from_slice(b"/v1");
    m.push(0x00);
    m.extend_from_slice(x);
    m
}

pub fn h(tag: &str, x: &[u8]) -> [u8; 32] {
    Sha256::digest(msg(tag, x)).into()
}

/// `gen_start(lineage, gen) = H_genstart(lineage || gen_be64)` (§9.3).
pub fn gen_start(lineage: &[u8; 16], gen: u64) -> [u8; 32] {
    let mut p = Vec::with_capacity(24);
    p.extend_from_slice(lineage);
    p.extend_from_slice(&gen.to_be_bytes());
    h("genstart", &p)
}

/// `key_id = H_key({alg, pk})` — the reducer builds the tiny
/// canonical two-entry map itself. Canonical order is by ENCODED key
/// bytes: `"pk"` encodes `62 70 6b`, `"alg"` encodes `63 61 6c 67`,
/// and `0x62 < 0x63` — so `pk` sorts FIRST (the shorter key wins
/// through its header byte, not alphabetically).
pub fn key_id(alg: &str, pk: &[u8]) -> [u8; 32] {
    let mut m = vec![0xa2u8];
    // "pk"
    m.push(0x62);
    m.extend_from_slice(b"pk");
    m.extend_from_slice(&bytes_header(pk.len()));
    m.extend_from_slice(pk);
    // "alg"
    m.push(0x63);
    m.extend_from_slice(b"alg");
    m.extend_from_slice(&text_header(alg.len()));
    m.extend_from_slice(alg.as_bytes());
    h("key", &m)
}

/// §11.8 bundle leaf: `H_brec(canonical bundleleaf)` where
/// `bundleleaf = { v: 1, export_id, rec_index, rec: bundlerec }` and
/// `bundlerec = { v: 1, op, kind, statement, class_floor }` — the
/// reducer hand-encodes both maps in canonical (encoded-byte) key
/// order: `v < rec < export_id < rec_index` and
/// `v < op < kind < statement < class_floor`.
pub fn brec_leaf(
    export_id: &[u8; 16],
    rec_index: u64,
    op: &[u8; 32],
    kind: &str,
    statement: &str,
    class_floor: &str,
) -> [u8; 32] {
    let mut rec = vec![0xa5u8];
    rec.push(0x61);
    rec.extend_from_slice(b"v");
    rec.extend_from_slice(&uint_bytes(1));
    rec.push(0x62);
    rec.extend_from_slice(b"op");
    rec.extend_from_slice(&bytes_header(op.len()));
    rec.extend_from_slice(op);
    rec.push(0x64);
    rec.extend_from_slice(b"kind");
    rec.extend_from_slice(&text_header(kind.len()));
    rec.extend_from_slice(kind.as_bytes());
    rec.extend_from_slice(&text_header("statement".len()));
    rec.extend_from_slice(b"statement");
    rec.extend_from_slice(&text_header(statement.len()));
    rec.extend_from_slice(statement.as_bytes());
    rec.extend_from_slice(&text_header("class_floor".len()));
    rec.extend_from_slice(b"class_floor");
    rec.extend_from_slice(&text_header(class_floor.len()));
    rec.extend_from_slice(class_floor.as_bytes());

    let mut leaf = vec![0xa4u8];
    leaf.push(0x61);
    leaf.extend_from_slice(b"v");
    leaf.extend_from_slice(&uint_bytes(1));
    leaf.push(0x63);
    leaf.extend_from_slice(b"rec");
    leaf.extend_from_slice(&rec);
    leaf.extend_from_slice(&text_header("export_id".len()));
    leaf.extend_from_slice(b"export_id");
    leaf.extend_from_slice(&bytes_header(export_id.len()));
    leaf.extend_from_slice(export_id);
    leaf.extend_from_slice(&text_header("rec_index".len()));
    leaf.extend_from_slice(b"rec_index");
    leaf.extend_from_slice(&uint_bytes(rec_index));
    h("brec", &leaf)
}

/// Fold a leaf up the §11.8 Merkle path: siblings bottom-up, exact
/// consumption, the trailing odd node promoting unchanged. Level
/// widths derive from `record_count` (a signed release fact). `None`
/// on leftover/missing siblings.
pub fn merkle_fold(
    leaf: [u8; 32],
    rec_index: u64,
    record_count: u64,
    proof: &[[u8; 32]],
) -> Option<[u8; 32]> {
    if record_count == 0 || rec_index >= record_count {
        return None;
    }
    let mut cur = leaf;
    let mut idx = rec_index;
    let mut width = record_count;
    let mut sibs = proof.iter();
    while width > 1 {
        if idx == width - 1 && width % 2 == 1 {
            // The trailing odd node promotes unchanged.
        } else {
            let sib = sibs.next()?;
            let mut cat = [0u8; 64];
            if idx.is_multiple_of(2) {
                cat[..32].copy_from_slice(&cur);
                cat[32..].copy_from_slice(sib);
            } else {
                cat[..32].copy_from_slice(sib);
                cat[32..].copy_from_slice(&cur);
            }
            cur = h("bnode", &cat);
        }
        idx /= 2;
        width = width.div_ceil(2);
    }
    // Exact consumption: leftover siblings fail.
    sibs.next().is_none().then_some(cur)
}

fn uint_bytes(n: u64) -> Vec<u8> {
    header(0, n as usize)
}

fn header(major: u8, n: usize) -> Vec<u8> {
    let n = n as u64;
    let mt = major << 5;
    if n < 24 {
        vec![mt | n as u8]
    } else if n <= u8::MAX as u64 {
        vec![mt | 24, n as u8]
    } else if n <= u16::MAX as u64 {
        let b = (n as u16).to_be_bytes();
        vec![mt | 25, b[0], b[1]]
    } else {
        let b = (n as u32).to_be_bytes();
        vec![mt | 26, b[0], b[1], b[2], b[3]]
    }
}

fn text_header(n: usize) -> Vec<u8> {
    header(3, n)
}

fn bytes_header(n: usize) -> Vec<u8> {
    header(2, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn frame_and_inventory() {
        assert_eq!(msg("op", b"abc"), b"intendant/op/v1\x00abc");
        assert_eq!(TAGS.len(), 25);
        assert_eq!(TAGS.iter().collect::<BTreeSet<_>>().len(), 25);
        // Distinct digests per tag.
        let mut seen = BTreeSet::new();
        for t in TAGS {
            seen.insert(h(t, b"same"));
        }
        assert_eq!(seen.len(), 25);
    }

    /// The reducer's own companion drift gate.
    #[test]
    fn companion_hash_domain_gate() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("d0a-vector-cases.v1.json");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut schema_tags = None;
        for c in v["$defs"]["case_contracts"]["allOf"].as_array().unwrap() {
            if c["if"]["properties"]["case_kind"]["const"] == "hash-domain" {
                schema_tags = Some(
                    c["then"]["properties"]["inputs"]["properties"]["tag"]["enum"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|t| t.as_str().unwrap().to_string())
                        .collect::<BTreeSet<_>>(),
                );
            }
        }
        let ours: BTreeSet<String> = TAGS.iter().map(|t| t.to_string()).collect();
        assert_eq!(schema_tags.unwrap(), ours);
    }

    /// key_id's inner map bytes are canonical (checked through the
    /// reducer's own strict reader — this test caught the first
    /// draft putting "alg" first: encoded-byte order sorts "pk"
    /// ahead of it).
    #[test]
    fn key_id_preimage_is_canonical() {
        // Rebuild the preimage the same way key_id does and decode it.
        let alg = "ed25519";
        let pk = [7u8; 32];
        let mut m = vec![0xa2u8];
        m.push(0x62);
        m.extend_from_slice(b"pk");
        m.extend_from_slice(&bytes_header(pk.len()));
        m.extend_from_slice(&pk);
        m.push(0x63);
        m.extend_from_slice(b"alg");
        m.extend_from_slice(&text_header(alg.len()));
        m.extend_from_slice(alg.as_bytes());
        let n = crate::cbor::decode(&m).unwrap();
        assert_eq!(n.map_keys(), Some(vec!["pk", "alg"]));
        assert_eq!(n.get("alg").unwrap().as_text(), Some(alg));
        assert_eq!(n.get("pk").unwrap().as_bytes(), Some(&pk[..]));
    }
}
