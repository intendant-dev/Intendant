//! Hash/signature domains — spec §2.
//!
//! `msg(tag, x) = "intendant/" || tag || "/v1" || 0x00 || canonical(x)`
//! `H_tag(x)    = SHA-256(msg(tag, x))`
//!
//! The tag inventory is CLOSED (E10): a new tag is a spec change,
//! never a fixture or implementation change. The `Tag` enum is the
//! only way to name a domain, and the companion-schema drift gate
//! (tests below) pins this enum set-equal to `d0a-vector-cases.v1.json`'s
//! `hash-domain` contract, so either side changing alone fails the
//! suite instead of shipping as drift.

use sha2::{Digest, Sha256};

/// The closed hash-domain inventory, in the spec's §2 order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tag {
    Genesis,
    Cert,
    Grant,
    Op,
    Body,
    Frontier,
    Receipt,
    Lease,
    Drill,
    Kek,
    Item,
    Policy,
    Key,
    Evrec,
    Genstart,
    Assertreq,
    Reauth,
    Cutoffreq,
    Abandonreq,
    Stmtid,
    Recips,
    Survivors,
    Brec,
    Bnode,
    Mat,
}

impl Tag {
    /// Every live tag, in §2 inventory order.
    pub const ALL: [Tag; 25] = [
        Tag::Genesis,
        Tag::Cert,
        Tag::Grant,
        Tag::Op,
        Tag::Body,
        Tag::Frontier,
        Tag::Receipt,
        Tag::Lease,
        Tag::Drill,
        Tag::Kek,
        Tag::Item,
        Tag::Policy,
        Tag::Key,
        Tag::Evrec,
        Tag::Genstart,
        Tag::Assertreq,
        Tag::Reauth,
        Tag::Cutoffreq,
        Tag::Abandonreq,
        Tag::Stmtid,
        Tag::Recips,
        Tag::Survivors,
        Tag::Brec,
        Tag::Bnode,
        Tag::Mat,
    ];

    /// The wire name — the exact byte string framed into `msg()`.
    pub fn name(self) -> &'static str {
        match self {
            Tag::Genesis => "genesis",
            Tag::Cert => "cert",
            Tag::Grant => "grant",
            Tag::Op => "op",
            Tag::Body => "body",
            Tag::Frontier => "frontier",
            Tag::Receipt => "receipt",
            Tag::Lease => "lease",
            Tag::Drill => "drill",
            Tag::Kek => "kek",
            Tag::Item => "item",
            Tag::Policy => "policy",
            Tag::Key => "key",
            Tag::Evrec => "evrec",
            Tag::Genstart => "genstart",
            Tag::Assertreq => "assertreq",
            Tag::Reauth => "reauth",
            Tag::Cutoffreq => "cutoffreq",
            Tag::Abandonreq => "abandonreq",
            Tag::Stmtid => "stmtid",
            Tag::Recips => "recips",
            Tag::Survivors => "survivors",
            Tag::Brec => "brec",
            Tag::Bnode => "bnode",
            Tag::Mat => "mat",
        }
    }

    pub fn from_name(s: &str) -> Option<Tag> {
        Tag::ALL.iter().copied().find(|t| t.name() == s)
    }
}

/// The domain frame: `"intendant/" || tag || "/v1" || 0x00 || x`.
///
/// `x` must already be the canonical encoding (§1) of the tagged
/// object — this function frames, it does not canonicalize.
pub fn msg(tag: Tag, x: &[u8]) -> Vec<u8> {
    let name = tag.name();
    let mut m = Vec::with_capacity(10 + name.len() + 4 + x.len());
    m.extend_from_slice(b"intendant/");
    m.extend_from_slice(name.as_bytes());
    m.extend_from_slice(b"/v1");
    m.push(0x00);
    m.extend_from_slice(x);
    m
}

/// `H_tag(x) = SHA-256(msg(tag, x))`.
pub fn h_tag(tag: Tag, x: &[u8]) -> [u8; 32] {
    Sha256::digest(msg(tag, x)).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn frame_bytes_pinned() {
        assert_eq!(msg(Tag::Op, b"abc"), b"intendant/op/v1\x00abc");
        assert_eq!(msg(Tag::Mat, b""), b"intendant/mat/v1\x00");
    }

    #[test]
    fn sha256_nist_sanity() {
        // FIPS 180-2 "abc" vector — pins the sha2 dependency itself.
        assert_eq!(
            hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn cross_tag_separation() {
        // The same input under every domain: 25 distinct digests, and
        // none equal to the unframed hash.
        let x = b"same input";
        let mut seen: BTreeSet<[u8; 32]> = BTreeSet::new();
        for t in Tag::ALL {
            seen.insert(h_tag(t, x));
        }
        assert_eq!(seen.len(), 25);
        assert!(!seen.contains::<[u8; 32]>(&Sha256::digest(x).into()));
    }

    #[test]
    fn inventory_names_closed_and_roundtrip() {
        assert_eq!(Tag::ALL.len(), 25);
        let names: BTreeSet<&str> = Tag::ALL.iter().map(|t| t.name()).collect();
        assert_eq!(names.len(), 25, "duplicate wire name");
        for t in Tag::ALL {
            assert_eq!(Tag::from_name(t.name()), Some(t));
            assert!(t.name().bytes().all(|b| b.is_ascii_lowercase()));
        }
        assert_eq!(
            Tag::from_name("bundle"),
            None,
            "retired tag must not resolve"
        );
        assert_eq!(Tag::from_name("bhdr"), None, "retired tag must not resolve");
    }

    /// The drift gate: this enum ≡ the companion schema's `hash-domain`
    /// contract tag enum (`$defs.case_contracts.allOf[]` where
    /// `if.properties.case_kind.const == "hash-domain"`).
    #[test]
    fn companion_schema_drift_gate() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("owner-plane-reducer")
            .join("corpus")
            .join("d0a-vector-cases.v1.json");
        let text = std::fs::read_to_string(&path).expect("companion schema readable");
        let v: serde_json::Value = serde_json::from_str(&text).expect("companion parses");

        let contracts = v["$defs"]["case_contracts"]["allOf"]
            .as_array()
            .expect("$defs.case_contracts.allOf is an array");
        let mut schema_tags: Option<BTreeSet<String>> = None;
        for c in contracts {
            if c["if"]["properties"]["case_kind"]["const"] == "hash-domain" {
                let e = c["then"]["properties"]["inputs"]["properties"]["tag"]["enum"]
                    .as_array()
                    .expect("hash-domain contract carries inputs.tag.enum");
                assert!(schema_tags.is_none(), "duplicate hash-domain contract");
                schema_tags = Some(
                    e.iter()
                        .map(|t| {
                            t.as_str()
                                .expect("tag enum entries are strings")
                                .to_string()
                        })
                        .collect(),
                );
            }
        }
        let schema_tags = schema_tags.expect("companion has a hash-domain contract");
        let enum_tags: BTreeSet<String> = Tag::ALL.iter().map(|t| t.name().to_string()).collect();
        assert_eq!(
            schema_tags, enum_tags,
            "Tag enum and the companion hash-domain enum diverged"
        );
    }
}
