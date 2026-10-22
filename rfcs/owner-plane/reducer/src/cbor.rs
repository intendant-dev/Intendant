//! The strict canonical-CBOR reader — E1–E10's decoder side (§1).
//!
//! The profile is closed: unsigned integers ≤ 2^53 − 1 (E1), byte and
//! text strings (definite-length only), arrays, maps (keys sorted
//! bytewise by ENCODED key, duplicates illegal — E7 reader side),
//! and the two booleans. Nothing else exists — no negatives, floats,
//! tags, null/undefined, or indefinite lengths. Every header must be
//! shortest-form. Container nesting is capped at 8 CONTAINER levels
//! (E8 — a leaf adds none). E9: this is the strict raw decoder lane —
//! a document that decodes here is canonical by construction, so
//! `re-encode(parse(bytes)) == bytes` needs no re-encoder.
//!
//! Every decoded node carries its exact input slice (`raw`), so the
//! reducer hashes sub-objects (bodies, headers, carried certs and
//! grants) directly from the bytes it verified — no writer, no
//! re-serialization, no verify-after-reserialize.

/// E1: the uint ceiling, 2^53 − 1.
pub const E1_MAX_UINT: u64 = (1 << 53) - 1;
/// E8: maximum container nesting depth (container levels only).
pub const MAX_DEPTH: usize = 8;

/// Why a document failed the strict reader. `Malformed` = not valid
/// CBOR in the profile at all; `NonCanonical` = valid CBOR bytes
/// that violate a canonicality rule (shortest form, key order,
/// duplicate keys); `Depth` = the E8 cap; `UintRange` = E1.
/// The caller maps these to §10.4 outcomes (`malformed`,
/// `non-canonical`, `depth` — `UintRange` is `non-canonical`'s E1
/// sibling and maps to `malformed`'s family per E10; the fold layer
/// owns that mapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    Malformed,
    NonCanonical,
    Depth,
    UintRange,
    /// Input continued past the single top-level item.
    TrailingBytes,
}

/// One decoded node with its exact encoded slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node<'a> {
    pub value: Value<'a>,
    pub raw: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value<'a> {
    Uint(u64),
    Bytes(&'a [u8]),
    Text(&'a str),
    Array(Vec<Node<'a>>),
    /// Entries in encoded order (which strictness proves is sorted
    /// and duplicate-free).
    Map(Vec<(Node<'a>, Node<'a>)>),
    Bool(bool),
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, DecodeError> {
        let b = *self.buf.get(self.pos).ok_or(DecodeError::Malformed)?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError::Malformed)?;
        let s = self.buf.get(self.pos..end).ok_or(DecodeError::Malformed)?;
        self.pos = end;
        Ok(s)
    }

    /// Read a header's argument, enforcing shortest form.
    fn argument(&mut self, info: u8) -> Result<u64, DecodeError> {
        match info {
            0..=23 => Ok(info as u64),
            24 => {
                let v = self.byte()? as u64;
                if v < 24 {
                    return Err(DecodeError::NonCanonical);
                }
                Ok(v)
            }
            25 => {
                let b = self.take(2)?;
                let v = u16::from_be_bytes([b[0], b[1]]) as u64;
                if v <= u8::MAX as u64 {
                    return Err(DecodeError::NonCanonical);
                }
                Ok(v)
            }
            26 => {
                let b = self.take(4)?;
                let v = u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64;
                if v <= u16::MAX as u64 {
                    return Err(DecodeError::NonCanonical);
                }
                Ok(v)
            }
            27 => {
                let b = self.take(8)?;
                let v = u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
                if v <= u32::MAX as u64 {
                    return Err(DecodeError::NonCanonical);
                }
                Ok(v)
            }
            // 28–30 reserved; 31 = indefinite length — outside the
            // profile.
            _ => Err(DecodeError::Malformed),
        }
    }

    fn item(&mut self, depth: usize) -> Result<Node<'a>, DecodeError> {
        let start = self.pos;
        let ib = self.byte()?;
        let (major, info) = (ib >> 5, ib & 0x1f);
        let value = match major {
            0 => {
                let v = self.argument(info)?;
                if v > E1_MAX_UINT {
                    return Err(DecodeError::UintRange);
                }
                Value::Uint(v)
            }
            2 => {
                let n = self.argument(info)?;
                let n = usize::try_from(n).map_err(|_| DecodeError::Malformed)?;
                Value::Bytes(self.take(n)?)
            }
            3 => {
                let n = self.argument(info)?;
                let n = usize::try_from(n).map_err(|_| DecodeError::Malformed)?;
                let s = self.take(n)?;
                Value::Text(core::str::from_utf8(s).map_err(|_| DecodeError::Malformed)?)
            }
            4 => {
                // E8 counts container levels; a leaf adds none.
                if depth + 1 > MAX_DEPTH {
                    return Err(DecodeError::Depth);
                }
                let n = self.argument(info)?;
                let n = usize::try_from(n).map_err(|_| DecodeError::Malformed)?;
                let mut items = Vec::new();
                for _ in 0..n {
                    items.push(self.item(depth + 1)?);
                }
                Value::Array(items)
            }
            5 => {
                if depth + 1 > MAX_DEPTH {
                    return Err(DecodeError::Depth);
                }
                let n = self.argument(info)?;
                let n = usize::try_from(n).map_err(|_| DecodeError::Malformed)?;
                let mut entries: Vec<(Node<'a>, Node<'a>)> = Vec::new();
                let mut prev_key: Option<&'a [u8]> = None;
                for _ in 0..n {
                    let k = self.item(depth + 1)?;
                    // E7 reader side: strictly ascending encoded key
                    // bytes — equal keys are duplicates, descending
                    // is unsorted; both non-canonical.
                    if let Some(p) = prev_key {
                        if k.raw <= p {
                            return Err(DecodeError::NonCanonical);
                        }
                    }
                    prev_key = Some(k.raw);
                    let v = self.item(depth + 1)?;
                    entries.push((k, v));
                }
                Value::Map(entries)
            }
            7 => match info {
                20 => Value::Bool(false),
                21 => Value::Bool(true),
                // null/undefined/floats/other simples: outside the
                // profile.
                _ => return Err(DecodeError::Malformed),
            },
            // Major 1 (negatives) and 6 (tags): outside the profile.
            _ => return Err(DecodeError::Malformed),
        };
        Ok(Node {
            value,
            raw: &self.buf[start..self.pos],
        })
    }
}

/// Decode exactly one canonical item spanning the whole input.
pub fn decode(buf: &[u8]) -> Result<Node<'_>, DecodeError> {
    let mut r = Reader { buf, pos: 0 };
    let node = r.item(0)?;
    if r.pos != buf.len() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(node)
}

impl<'a> Node<'a> {
    /// Map lookup by text key (maps are already proven sorted and
    /// duplicate-free, so linear scan is exact).
    pub fn get(&self, key: &str) -> Option<&Node<'a>> {
        if let Value::Map(entries) = &self.value {
            for (k, v) in entries {
                if let Value::Text(t) = k.value {
                    if t == key {
                        return Some(v);
                    }
                }
            }
        }
        None
    }

    pub fn as_uint(&self) -> Option<u64> {
        match self.value {
            Value::Uint(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        match self.value {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&'a str> {
        match self.value {
            Value::Text(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Node<'a>]> {
        match &self.value {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&[(Node<'a>, Node<'a>)]> {
        match &self.value {
            Value::Map(entries) => Some(entries),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self.value {
            Value::Bool(b) => Some(b),
            _ => None,
        }
    }

    /// Fixed-length byte field.
    pub fn bytes_n<const N: usize>(&self) -> Option<[u8; N]> {
        self.as_bytes()?.try_into().ok()
    }

    /// The map's text keys, in encoded (= sorted) order — shape
    /// checks compare these against a production's closed key set.
    pub fn map_keys(&self) -> Option<Vec<&'a str>> {
        let entries = self.as_map()?;
        let mut keys = Vec::with_capacity(entries.len());
        for (k, _) in entries {
            keys.push(k.as_text()?);
        }
        Some(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hx(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn rfc8949_appendix_a_positives() {
        // Integers at every width boundary.
        for (h, v) in [
            ("00", 0u64),
            ("17", 23),
            ("1818", 24),
            ("18ff", 255),
            ("190100", 256),
            ("19ffff", 65535),
            ("1a00010000", 65536),
            ("1affffffff", 4294967295),
            ("1b0000000100000000", 4294967296),
        ] {
            let b = hx(h);
            let n = decode(&b).unwrap();
            assert_eq!(n.as_uint(), Some(v), "{h}");
            assert_eq!(n.raw, &b[..]);
        }
        // E1 ceiling exactly.
        let b = hx("1b001fffffffffffff");
        assert_eq!(decode(&b).unwrap().as_uint(), Some(E1_MAX_UINT));

        // "IETF", h'01020304', [1, [2, 3]], booleans.
        let b = hx("6449455446");
        assert_eq!(decode(&b).unwrap().as_text(), Some("IETF"));
        let b = hx("4401020304");
        assert_eq!(decode(&b).unwrap().as_bytes(), Some(&hx("01020304")[..]));
        let b = hx("8201820203");
        let n = decode(&b).unwrap();
        let items = n.as_array().unwrap();
        assert_eq!(items[0].as_uint(), Some(1));
        assert_eq!(items[1].as_array().unwrap()[1].as_uint(), Some(3));
        assert_eq!(decode(&hx("f4")).unwrap().as_bool(), Some(false));
        assert_eq!(decode(&hx("f5")).unwrap().as_bool(), Some(true));

        // {"a": 1, "b": [2, 3]} — canonical order.
        let b = hx("a26161016162820203");
        let n = decode(&b).unwrap();
        assert_eq!(n.get("a").unwrap().as_uint(), Some(1));
        assert_eq!(n.map_keys(), Some(vec!["a", "b"]));
    }

    #[test]
    fn raw_spans_are_exact_sub_slices() {
        // {"a": h'0102', "b": {"c": 5}}
        let b = hx("a261614201026162a1616305");
        let n = decode(&b).unwrap();
        assert_eq!(n.raw, &b[..]);
        assert_eq!(n.get("a").unwrap().raw, &hx("420102")[..]);
        let inner = n.get("b").unwrap();
        assert_eq!(inner.raw, &hx("a1616305")[..]);
        assert_eq!(inner.get("c").unwrap().raw, &hx("05")[..]);
    }

    #[test]
    fn shortest_form_enforced_every_width() {
        for h in [
            "1800",               // 0 as one-byte argument
            "1817",               // 23 as one-byte argument
            "190018",             // 24 as two-byte
            "1900ff",             // 255 as two-byte
            "1a0000ffff",         // 65535 as four-byte
            "1b00000000ffffffff", // 2^32-1 as eight-byte
            "5800",               // empty bstr with one-byte length
            "7800",               // empty tstr with one-byte length
            "9800",               // empty array with one-byte length
            "b800",               // empty map with one-byte length
        ] {
            assert_eq!(
                decode(&hx(h)),
                Err(DecodeError::NonCanonical),
                "{h} must be non-canonical"
            );
        }
    }

    #[test]
    fn e1_range_enforced() {
        // 2^53 — one past the ceiling.
        assert_eq!(
            decode(&hx("1b0020000000000000")),
            Err(DecodeError::UintRange)
        );
        assert_eq!(
            decode(&hx("1bffffffffffffffff")),
            Err(DecodeError::UintRange)
        );
    }

    #[test]
    fn profile_exclusions_are_malformed() {
        for h in [
            "20",                 // -1 (major 1)
            "c000",               // tag 0 (major 6)
            "f6",                 // null
            "f7",                 // undefined
            "f97e00",             // float16 NaN
            "fb3ff0000000000000", // float64 1.0
            "5f42010243030405ff", // indefinite bstr
            "9fff",               // indefinite array
            "bfff",               // indefinite map
            "ff",                 // lone break
        ] {
            assert_eq!(decode(&hx(h)), Err(DecodeError::Malformed), "{h}");
        }
    }

    #[test]
    fn map_order_and_duplicates() {
        // {"b": 1, "a": 2} — unsorted.
        assert_eq!(
            decode(&hx("a2616201616102")),
            Err(DecodeError::NonCanonical)
        );
        // {"a": 1, "a": 2} — duplicate.
        assert_eq!(
            decode(&hx("a2616101616102")),
            Err(DecodeError::NonCanonical)
        );
    }

    #[test]
    fn map_sorts_by_encoded_bytes_not_length() {
        // {"b": 1, "aa": 2}: "b" encodes 6162, "aa" encodes 626161;
        // 0x61… < 0x62… bytewise, so "b" first IS canonical (the
        // shorter key wins here through its first byte, not its
        // length).
        let ok = hx("a261620162616102");
        let n = decode(&ok).unwrap();
        assert_eq!(n.map_keys(), Some(vec!["b", "aa"]));
        // The reverse order is non-canonical.
        let bad = hx("a26261610261 6201".replace(' ', "").as_str());
        assert_eq!(decode(&bad), Err(DecodeError::NonCanonical));
    }

    #[test]
    fn depth_counts_container_levels_only() {
        // 8 nested arrays holding a leaf: legal (8 container levels).
        let mut ok = vec![0x81u8; 8];
        ok.push(0x00);
        assert!(decode(&ok).is_ok());
        // 9 nested arrays: depth.
        let mut bad = vec![0x81u8; 9];
        bad.push(0x00);
        assert_eq!(decode(&bad), Err(DecodeError::Depth));
        // Map nesting counts the same: 7 arrays + map holding a leaf = 8.
        let mut ok2 = vec![0x81u8; 7];
        ok2.extend_from_slice(&hx("a1616100"));
        assert!(decode(&ok2).is_ok());
        let mut bad2 = vec![0x81u8; 8];
        bad2.extend_from_slice(&hx("a1616100"));
        assert_eq!(decode(&bad2), Err(DecodeError::Depth));
    }

    #[test]
    fn truncation_and_trailing() {
        assert_eq!(decode(&hx("1a0001")), Err(DecodeError::Malformed));
        assert_eq!(decode(&hx("62e6")), Err(DecodeError::Malformed)); // short tstr
        assert_eq!(decode(&hx("0000")), Err(DecodeError::TrailingBytes));
        assert_eq!(decode(&[]), Err(DecodeError::Malformed));
        // Invalid UTF-8 in a tstr.
        assert_eq!(decode(&hx("62c328")), Err(DecodeError::Malformed));
    }
}
