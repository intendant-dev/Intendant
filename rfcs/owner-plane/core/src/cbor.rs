//! Canonical CBOR encoding — the writer side of spec §1 (E1–E10).
//!
//! RFC 8949 §4.2.1 Core Deterministic Encoding, restricted per E1:
//! shortest-form unsigned integers, no floats, no tags, no indefinite
//! lengths; definite-length byte/text strings, arrays, and maps; map
//! keys sorted bytewise by their encoded form, duplicates rejected
//! (E7's logical-key layer sits ABOVE this encoder — this module is
//! byte-level canonicality only).

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// E1: integers are unsigned unless a shape states otherwise.
    Uint(u64),
    /// CDDL `bool` — simple values 20/21 (E4 excludes only
    /// null/undefined/tags/floats).
    Bool(bool),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Value>),
    /// Map entries are supplied in any order; encoding sorts by the
    /// canonical encoded key and rejects duplicate encoded keys.
    Map(Vec<(Value, Value)>),
}

#[derive(Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// Two map entries whose keys encode to identical bytes.
    DuplicateKey,
    /// Nesting beyond the E8 depth cap (8).
    DepthExceeded,
}

// E8: nesting depth ≤ 8. The cap counts CONTAINER levels — a leaf
// inside the innermost container adds none (counting leaves would make
// an empty and an occupied innermost container diverge at the cap).
const MAX_DEPTH: usize = 8;

/// Encode the major-type header with the shortest-form argument (E1).
fn header(major: u8, arg: u64, out: &mut Vec<u8>) {
    let mt = major << 5;
    match arg {
        0..=23 => out.push(mt | arg as u8),
        24..=0xff => {
            out.push(mt | 24);
            out.push(arg as u8);
        }
        0x100..=0xffff => {
            out.push(mt | 25);
            out.extend_from_slice(&(arg as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(mt | 26);
            out.extend_from_slice(&(arg as u32).to_be_bytes());
        }
        _ => {
            out.push(mt | 27);
            out.extend_from_slice(&arg.to_be_bytes());
        }
    }
}

/// `depth` = the container level this value's own container occupies if
/// this value IS a container (the outermost sits at level 1); leaves
/// carry no level of their own and are never depth-checked.
fn encode_into(v: &Value, depth: usize, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    match v {
        Value::Uint(n) => header(0, *n, out),
        Value::Bool(b) => out.push(if *b { 0xf5 } else { 0xf4 }),
        Value::Bytes(b) => {
            header(2, b.len() as u64, out);
            out.extend_from_slice(b);
        }
        Value::Text(s) => {
            header(3, s.len() as u64, out);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Array(items) => {
            if depth > MAX_DEPTH {
                return Err(EncodeError::DepthExceeded);
            }
            header(4, items.len() as u64, out);
            for item in items {
                encode_into(item, depth + 1, out)?;
            }
        }
        Value::Map(entries) => {
            if depth > MAX_DEPTH {
                return Err(EncodeError::DepthExceeded);
            }
            // Canonical order: bytewise over the ENCODED key (RFC 8949
            // §4.2.1 rule 2 — length-first falls out of the header
            // encoding). BTreeMap gives the sort and detects duplicates.
            let mut sorted: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
            for (k, val) in entries {
                let mut kb = Vec::new();
                encode_into(k, depth + 1, &mut kb)?;
                let mut vb = Vec::new();
                encode_into(val, depth + 1, &mut vb)?;
                if sorted.insert(kb, vb).is_some() {
                    return Err(EncodeError::DuplicateKey);
                }
            }
            header(5, sorted.len() as u64, out);
            for (kb, vb) in sorted {
                out.extend_from_slice(&kb);
                out.extend_from_slice(&vb);
            }
        }
    }
    Ok(())
}

/// Canonical encoding of `v` (E1-restricted RFC 8949 §4.2.1).
pub fn encode(v: &Value) -> Result<Vec<u8>, EncodeError> {
    let mut out = Vec::new();
    encode_into(v, 1, &mut out)?;
    Ok(out)
}

/// Convenience: a map from string keys (the spec's object shapes all
/// use text keys).
pub fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (Value::Text(k.to_string()), v))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn shortest_form_boundaries() {
        // RFC 8949 appendix-A pinned encodings.
        for (n, want) in [
            (0u64, "00"),
            (23, "17"),
            (24, "1818"),
            (255, "18ff"),
            (256, "190100"),
            (65535, "19ffff"),
            (65536, "1a00010000"),
            (4294967295, "1affffffff"),
            (4294967296, "1b0000000100000000"),
            (u64::MAX, "1bffffffffffffffff"),
        ] {
            assert_eq!(hex(&encode(&Value::Uint(n)).unwrap()), want, "n={n}");
        }
    }

    #[test]
    fn bools_pinned() {
        // RFC 8949 appendix A: false = f4, true = f5.
        assert_eq!(hex(&encode(&Value::Bool(false)).unwrap()), "f4");
        assert_eq!(hex(&encode(&Value::Bool(true)).unwrap()), "f5");
    }

    #[test]
    fn strings_and_arrays_pinned() {
        // RFC 8949 appendix A: "IETF" = 6449455446; h'01020304' = 4401020304.
        assert_eq!(
            hex(&encode(&Value::Text("IETF".into())).unwrap()),
            "6449455446"
        );
        assert_eq!(
            hex(&encode(&Value::Bytes(vec![1, 2, 3, 4])).unwrap()),
            "4401020304"
        );
        // [1, [2, 3]] = 8201820203
        let v = Value::Array(vec![
            Value::Uint(1),
            Value::Array(vec![Value::Uint(2), Value::Uint(3)]),
        ]);
        assert_eq!(hex(&encode(&v).unwrap()), "8201820203");
    }

    #[test]
    fn map_keys_sort_by_encoded_bytes_not_insertion() {
        // {"b": 1, "a": 2, 10: 3} — canonical order sorts the ENCODED
        // keys: 0x0a (10) < 0x61 'a' < 0x62 'b'.
        let v = Value::Map(vec![
            (Value::Text("b".into()), Value::Uint(1)),
            (Value::Text("a".into()), Value::Uint(2)),
            (Value::Uint(10), Value::Uint(3)),
        ]);
        assert_eq!(
            hex(&encode(&v).unwrap()),
            "a30a036161026162 01".replace(' ', "")
        );
    }

    #[test]
    fn duplicate_encoded_keys_reject() {
        let v = Value::Map(vec![
            (Value::Text("a".into()), Value::Uint(1)),
            (Value::Text("a".into()), Value::Uint(2)),
        ]);
        assert_eq!(encode(&v), Err(EncodeError::DuplicateKey));
    }

    #[test]
    fn depth_cap_enforced() {
        // E8: container nesting ≤ 8. Nine nested arrays reject.
        let mut v = Value::Uint(0);
        for _ in 0..9 {
            v = Value::Array(vec![v]);
        }
        assert_eq!(encode(&v), Err(EncodeError::DepthExceeded));
        // Eight containers around a leaf are fine — the leaf adds no
        // level (an occupied innermost container must not diverge from
        // an empty one at the cap).
        let mut v = Value::Uint(0);
        for _ in 0..8 {
            v = Value::Array(vec![v]);
        }
        assert!(encode(&v).is_ok());
        // Maps count as container levels too.
        let mut v = Value::Uint(0);
        for _ in 0..9 {
            v = map(vec![("k", v)]);
        }
        assert_eq!(encode(&v), Err(EncodeError::DepthExceeded));
    }

    #[test]
    fn encoding_is_insertion_order_independent() {
        let a = map(vec![("x", Value::Uint(1)), ("y", Value::Uint(2))]);
        let b = map(vec![("y", Value::Uint(2)), ("x", Value::Uint(1))]);
        assert_eq!(encode(&a).unwrap(), encode(&b).unwrap());
    }
}
