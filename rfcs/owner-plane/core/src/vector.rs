//! §13.1 vector-file emitter.
//!
//! Builds vector JSON conforming to the container schema (embedded
//! verbatim in the spec — pinned below) and enforces at mint time
//! what the schemas and §13.1 prose close: the container shape, the
//! §10.4×§10.5 outcome/disposition pairing (everywhere it appears),
//! the companion's family→case_kind vocabulary, draw-name uniqueness,
//! and the no-browser-signing-draws tier.
//!
//! Deliberately NOT here: per-case_kind `inputs`/`expected.result`
//! contract validation — that is the differential harness's job with
//! a real JSON Schema engine against the companion (independent code,
//! per the program order). Builders in this crate conform by typed
//! construction; the harness re-checks from the schemas alone.

use serde_json::{json, Map, Value};

use crate::outcomes;
use crate::rng::VectorRng;

/// The container schema's closed surface enum, in schema order.
pub const SURFACES: &[&str] = &[
    "core",
    "native-crypto",
    "browser",
    "storage-macos",
    "storage-linux",
    "storage-windows",
];

/// Lowercase hex of `b`.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn is_hex_lower(s: &str) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The §13.1 RNG discipline with draw bookkeeping: one ChaCha20
/// keystream; every draw is named; the emitted `draw_order` records
/// `{name, nbytes}` in exact draw order. Duplicate names and empty
/// draws panic — fixture bugs, not runtime conditions.
pub struct RecordingRng {
    key: [u8; 32],
    nonce: [u8; 12],
    rng: VectorRng,
    draws: Vec<(String, u64)>,
}

impl RecordingRng {
    pub fn new(key: [u8; 32], nonce: [u8; 12]) -> Self {
        RecordingRng {
            key,
            nonce,
            rng: VectorRng::new(&key, &nonce),
            draws: Vec::new(),
        }
    }

    pub fn draw(&mut self, name: &str, nbytes: usize) -> Vec<u8> {
        assert!(nbytes >= 1, "empty draw: {name}");
        assert!(
            self.draws.iter().all(|(n, _)| n != name),
            "duplicate draw name: {name}"
        );
        self.draws.push((name.to_string(), nbytes as u64));
        self.rng.draw(nbytes)
    }

    pub fn draw32(&mut self, name: &str) -> [u8; 32] {
        self.draw(name, 32).try_into().expect("32-byte draw")
    }

    pub fn draw16(&mut self, name: &str) -> [u8; 16] {
        self.draw(name, 16).try_into().expect("16-byte draw")
    }

    pub fn draw12(&mut self, name: &str) -> [u8; 12] {
        self.draw(name, 12).try_into().expect("12-byte draw")
    }

    /// The vector's `rng` block. Panics on zero draws — a vector
    /// that drew nothing must omit the block instead.
    pub fn into_json(self) -> Value {
        assert!(!self.draws.is_empty(), "rng block with no draws");
        json!({
            "algorithm": "chacha20",
            "key": hex(&self.key),
            "nonce": hex(&self.nonce),
            "draw_order": self.draws.iter().map(|(name, nbytes)| json!({
                "name": name,
                "nbytes": nbytes,
            })).collect::<Vec<_>>(),
        })
    }
}

/// The `expected` object's three closed shapes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expected {
    /// `{ bytes }` — exact output bytes.
    Bytes(Vec<u8>),
    /// `{ result }` — a per-case_kind typed result.
    Result(Value),
    /// `{ outcome, disposition }` — a negative case.
    Negative {
        outcome: String,
        disposition: String,
    },
}

/// One vector file.
#[derive(Debug, Clone)]
pub struct Vector {
    pub family: u8,
    pub name: String,
    pub case_kind: String,
    pub source: String,
    pub surfaces: Vec<String>,
    /// A `RecordingRng::into_json()` block, when the vector draws.
    pub rng: Option<Value>,
    pub inputs: Map<String, Value>,
    pub expected: Expected,
}

impl Vector {
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("family".into(), json!(self.family));
        m.insert("name".into(), json!(self.name));
        m.insert("case_kind".into(), json!(self.case_kind));
        m.insert("source".into(), json!(self.source));
        m.insert("surfaces".into(), json!(self.surfaces));
        if let Some(rng) = &self.rng {
            m.insert("rng".into(), rng.clone());
        }
        m.insert("inputs".into(), Value::Object(self.inputs.clone()));
        let expected = match &self.expected {
            Expected::Bytes(b) => json!({ "bytes": hex(b) }),
            Expected::Result(r) => json!({ "result": r }),
            Expected::Negative {
                outcome,
                disposition,
            } => json!({ "outcome": outcome, "disposition": disposition }),
        };
        m.insert("expected".into(), expected);
        Value::Object(m)
    }

    /// Deterministic file bytes: pretty JSON (sorted keys — the
    /// default serde_json map) plus a trailing newline.
    pub fn to_file_string(&self) -> String {
        let mut s = serde_json::to_string_pretty(&self.to_json()).expect("vector JSON serializes");
        s.push('\n');
        s
    }
}

/// Mint-time conformance: the container schema's constraints, the
/// §13.1 prose rules the schema cannot express, the §10.4×§10.5
/// pairing everywhere outcome/disposition appear, and the companion's
/// family→case_kind vocabulary + surface tiers. `companion` is the
/// parsed `d0a-vector-cases.v1.json`.
pub fn check(v: &Value, companion: &Value) -> Result<(), String> {
    check_container(v)?;
    check_vocabulary(v, companion)?;
    check_pairs(v)?;
    Ok(())
}

const TOP_KEYS: &[&str] = &[
    "family",
    "name",
    "case_kind",
    "source",
    "surfaces",
    "rng",
    "inputs",
    "expected",
];
const TOP_REQUIRED: &[&str] = &[
    "family",
    "name",
    "case_kind",
    "source",
    "surfaces",
    "inputs",
    "expected",
];

fn check_container(v: &Value) -> Result<(), String> {
    let obj = v.as_object().ok_or("vector is not an object")?;
    for k in obj.keys() {
        if !TOP_KEYS.contains(&k.as_str()) {
            return Err(format!("unknown top-level key: {k}"));
        }
    }
    for k in TOP_REQUIRED {
        if !obj.contains_key(*k) {
            return Err(format!("missing required key: {k}"));
        }
    }

    let family = obj["family"]
        .as_u64()
        .ok_or("family is not an unsigned integer")?;
    if !(1..=14).contains(&family) {
        return Err(format!("family out of range: {family}"));
    }
    let name = obj["name"].as_str().ok_or("name is not a string")?;
    if name.chars().count() > 120 {
        return Err("name exceeds 120 chars".into());
    }
    let case_kind = obj["case_kind"]
        .as_str()
        .ok_or("case_kind is not a string")?;
    if case_kind.chars().count() > 80 {
        return Err("case_kind exceeds 80 chars".into());
    }
    obj["source"].as_str().ok_or("source is not a string")?;

    let surfaces = obj["surfaces"]
        .as_array()
        .ok_or("surfaces is not an array")?;
    if surfaces.is_empty() {
        return Err("surfaces is empty".into());
    }
    let mut seen = Vec::new();
    for s in surfaces {
        let s = s.as_str().ok_or("surface is not a string")?;
        if !SURFACES.contains(&s) {
            return Err(format!("unknown surface: {s}"));
        }
        if seen.contains(&s) {
            return Err(format!("duplicate surface: {s}"));
        }
        seen.push(s);
    }

    if let Some(rng) = obj.get("rng") {
        check_rng(rng)?;
    }

    if !obj["inputs"].is_object() {
        return Err("inputs is not an object".into());
    }

    let expected = obj["expected"]
        .as_object()
        .ok_or("expected is not an object")?;
    for k in expected.keys() {
        if !["bytes", "result", "outcome", "disposition"].contains(&k.as_str()) {
            return Err(format!("unknown expected key: {k}"));
        }
    }
    let mut keys: Vec<&str> = expected.keys().map(|k| k.as_str()).collect();
    keys.sort_unstable();
    match keys.as_slice() {
        ["bytes"] => {
            let b = expected["bytes"].as_str().ok_or("bytes is not a string")?;
            if b.len() % 2 != 0 || !is_hex_lower(b) {
                return Err("expected.bytes is not lowercase even-length hex".into());
            }
        }
        ["result"] => {}
        ["disposition", "outcome"] => {
            expected["outcome"]
                .as_str()
                .ok_or("outcome is not a string")?;
            expected["disposition"]
                .as_str()
                .ok_or("disposition is not a string")?;
        }
        other => {
            return Err(format!(
                "expected must be exactly one of {{bytes}}, {{result}}, {{outcome, disposition}}; got keys {other:?}"
            ));
        }
    }
    Ok(())
}

fn check_rng(rng: &Value) -> Result<(), String> {
    let obj = rng.as_object().ok_or("rng is not an object")?;
    for k in obj.keys() {
        if !["algorithm", "key", "nonce", "draw_order"].contains(&k.as_str()) {
            return Err(format!("unknown rng key: {k}"));
        }
    }
    for k in ["algorithm", "key", "nonce", "draw_order"] {
        if !obj.contains_key(k) {
            return Err(format!("rng missing {k}"));
        }
    }
    if obj["algorithm"].as_str() != Some("chacha20") {
        return Err("rng.algorithm must be \"chacha20\"".into());
    }
    let key = obj["key"].as_str().ok_or("rng.key is not a string")?;
    if key.len() != 64 || !is_hex_lower(key) {
        return Err("rng.key is not 64 lowercase hex chars".into());
    }
    let nonce = obj["nonce"].as_str().ok_or("rng.nonce is not a string")?;
    if nonce.len() != 24 || !is_hex_lower(nonce) {
        return Err("rng.nonce is not 24 lowercase hex chars".into());
    }
    let draws = obj["draw_order"]
        .as_array()
        .ok_or("rng.draw_order is not an array")?;
    if draws.is_empty() {
        return Err("rng.draw_order is empty".into());
    }
    let mut names: Vec<&str> = Vec::new();
    for d in draws {
        let d = d.as_object().ok_or("draw entry is not an object")?;
        for k in d.keys() {
            if !["name", "nbytes"].contains(&k.as_str()) {
                return Err(format!("unknown draw key: {k}"));
            }
        }
        let name = d
            .get("name")
            .and_then(Value::as_str)
            .ok_or("draw name is not a string")?;
        let nbytes = d
            .get("nbytes")
            .and_then(Value::as_u64)
            .ok_or("draw nbytes is not an unsigned integer")?;
        if nbytes < 1 {
            return Err(format!("draw {name} has nbytes 0"));
        }
        // §13.1 prose: draw_order names are unique within a vector.
        if names.contains(&name) {
            return Err(format!("duplicate draw name: {name}"));
        }
        names.push(name);
    }
    Ok(())
}

/// The companion's family→case_kind vocabulary and the
/// sign-then-verify browser exclusion (browsers get fixed signatures
/// to verify, never signing draws).
fn check_vocabulary(v: &Value, companion: &Value) -> Result<(), String> {
    let family = v["family"].as_u64().ok_or("family missing")?;
    let case_kind = v["case_kind"].as_str().ok_or("case_kind missing")?;

    let tiers = companion["$defs"]["family_vocabulary"]["allOf"]
        .as_array()
        .ok_or("companion lacks $defs.family_vocabulary.allOf")?;
    let mut allowed: Option<Vec<&str>> = None;
    for t in tiers {
        if t["if"]["properties"]["family"]["const"].as_u64() == Some(family) {
            let e = t["then"]["properties"]["case_kind"]["enum"]
                .as_array()
                .ok_or("family tier lacks case_kind enum")?;
            if allowed.is_some() {
                return Err(format!("duplicate family tier for {family}"));
            }
            allowed = Some(e.iter().filter_map(Value::as_str).collect());
        }
    }
    let allowed = allowed.ok_or(format!("no case_kind vocabulary for family {family}"))?;
    if !allowed.contains(&case_kind) {
        return Err(format!(
            "case_kind {case_kind:?} not in family {family}'s vocabulary {allowed:?}"
        ));
    }

    if case_kind == "sign-then-verify" {
        let has_browser = v["surfaces"]
            .as_array()
            .map(|a| a.iter().any(|s| s.as_str() == Some("browser")))
            .unwrap_or(false);
        if has_browser {
            return Err("sign-then-verify vectors exclude the browser surface".into());
        }
    }
    Ok(())
}

/// §10.4×§10.5 pairing, everywhere: any object under `expected`
/// carrying `outcome`/`disposition` must carry BOTH as a legal pair
/// (the top-level negative shape and every per-item / per-record /
/// trace row alike; a row with neither asserts admission).
fn check_pairs(v: &Value) -> Result<(), String> {
    fn walk(node: &Value, path: &str) -> Result<(), String> {
        match node {
            Value::Object(m) => {
                let o = m.get("outcome");
                let d = m.get("disposition");
                match (o, d) {
                    (None, None) => {}
                    (Some(o), Some(d)) => {
                        let o = o
                            .as_str()
                            .ok_or(format!("{path}: outcome is not a string"))?;
                        let d = d
                            .as_str()
                            .ok_or(format!("{path}: disposition is not a string"))?;
                        if !outcomes::valid_pair(o, d) {
                            return Err(format!(
                                "{path}: ({o:?}, {d:?}) is not a legal §10.4×§10.5 pair"
                            ));
                        }
                    }
                    _ => {
                        return Err(format!("{path}: outcome/disposition must appear as a pair"));
                    }
                }
                for (k, child) in m {
                    walk(child, &format!("{path}.{k}"))?;
                }
                Ok(())
            }
            Value::Array(a) => {
                for (i, child) in a.iter().enumerate() {
                    walk(child, &format!("{path}[{i}]"))?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    walk(&v["expected"], "expected")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shapes::assert_pins;

    /// The container schema, byte-for-byte as embedded in §13.1.
    const CONTAINER_SCHEMA: &str = r#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://intendant.dev/schemas/d0a-vector.v1.json",
  "type": "object",
  "additionalProperties": false,
  "required": ["family", "name", "case_kind", "source", "surfaces", "inputs", "expected"],
  "properties": {
    "family": { "type": "integer", "minimum": 1, "maximum": 14 },
    "name": { "type": "string", "maxLength": 120 },
    "case_kind": { "type": "string", "maxLength": 80 },
    "source": { "type": "string", "description": "normative section, e.g. 9.1" },
    "surfaces": {
      "type": "array", "minItems": 1, "uniqueItems": true,
      "items": { "enum": ["core", "native-crypto", "browser", "storage-macos", "storage-linux", "storage-windows"] }
    },
    "rng": {
      "type": "object", "additionalProperties": false,
      "required": ["algorithm", "key", "nonce", "draw_order"],
      "properties": {
        "algorithm": { "const": "chacha20" },
        "key": { "type": "string", "pattern": "^[0-9a-f]{64}$" },
        "nonce": { "type": "string", "pattern": "^[0-9a-f]{24}$" },
        "draw_order": {
          "type": "array", "minItems": 1,
          "items": {
            "type": "object", "additionalProperties": false,
            "required": ["name", "nbytes"],
            "properties": {
              "name": { "type": "string" },
              "nbytes": { "type": "integer", "minimum": 1 }
            }
          }
        }
      }
    },
    "inputs": { "type": "object" },
    "expected": {
      "type": "object", "additionalProperties": false,
      "properties": {
        "bytes": { "type": "string", "pattern": "^([0-9a-f]{2})*$" },
        "result": {},
        "outcome": { "type": "string" },
        "disposition": { "type": "string" }
      },
      "oneOf": [
        { "required": ["bytes"] },
        { "required": ["result"] },
        { "required": ["outcome", "disposition"] }
      ]
    }
  }
}"#;

    /// §13.1 prose rules this module enforces, verbatim.
    const SPEC_PINS: &[&str] = &[
        "Conventions: byte inputs are lowercase even-length hex; integers stay
within E1 ranges; text is UTF-8. The RNG is **ChaCha20 (RFC 8439)**
with the given 32-byte key and 12-byte nonce, initial counter 0; the
keystream is one byte stream, draws taken in the order given by the
vector's `draw_order` array of `{name, nbytes}` entries (explicit
names AND byte counts — JSON object property order is not portable,
and draw sizes must never depend on schema inference).",
        "`outcome`/`disposition` stay plain strings in the
schema deliberately: the harness cross-validates them against
§10.4/§10.5, so there is no duplicated enum to drift.",
        "every
vector must validate against container AND companion (a vector
failing either is invalid — a harness never invents family
semantics)",
        "`draw_order` names are unique within
a vector.",
        "Where browser WebCrypto cannot inject randomness (P-256
signing), vectors supply **fixed signatures to verify**, never signing
draws. Every negative vector asserts outcome **and** disposition
(§10.5).",
    ];

    #[test]
    fn spec_pins_are_verbatim() {
        assert_pins(SPEC_PINS);
        assert_pins(&[CONTAINER_SCHEMA]);
    }

    fn companion() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("d0a-vector-cases.v1.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    /// Derive-side assurance: the hardcoded validator constants match
    /// the pinned schema block parsed as JSON.
    #[test]
    fn validator_matches_container_schema() {
        let s: Value = serde_json::from_str(CONTAINER_SCHEMA).unwrap();
        let props: Vec<&str> = s["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(props.len(), TOP_KEYS.len());
        for k in TOP_KEYS {
            assert!(props.contains(k), "validator key {k} not in schema");
        }
        let req: Vec<&str> = s["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(req, TOP_REQUIRED);
        let surf: Vec<&str> = s["properties"]["surfaces"]["items"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(surf, SURFACES);
        assert_eq!(s["properties"]["family"]["minimum"], 1);
        assert_eq!(s["properties"]["family"]["maximum"], 14);
        assert_eq!(s["properties"]["name"]["maxLength"], 120);
        assert_eq!(s["properties"]["case_kind"]["maxLength"], 80);
        assert_eq!(
            s["properties"]["rng"]["properties"]["key"]["pattern"],
            "^[0-9a-f]{64}$"
        );
        assert_eq!(
            s["properties"]["rng"]["properties"]["nonce"]["pattern"],
            "^[0-9a-f]{24}$"
        );
        assert_eq!(
            s["properties"]["expected"]["properties"]["bytes"]["pattern"],
            "^([0-9a-f]{2})*$"
        );
        assert_eq!(
            s["properties"]["expected"]["oneOf"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    /// The hardcoded sign-then-verify browser exclusion is still
    /// exactly the companion's third top-level tier.
    #[test]
    fn browser_signing_tier_matches_companion() {
        let c = companion();
        let tier = &c["allOf"].as_array().unwrap()[2];
        assert_eq!(
            tier["if"]["properties"]["case_kind"]["const"],
            "sign-then-verify"
        );
        assert_eq!(
            tier["then"]["properties"]["surfaces"]["items"]["not"]["const"],
            "browser"
        );
    }

    /// The companion closes a case_kind vocabulary for every family.
    #[test]
    fn family_vocabulary_total() {
        let c = companion();
        let tiers = c["$defs"]["family_vocabulary"]["allOf"].as_array().unwrap();
        let mut families: Vec<u64> = tiers
            .iter()
            .map(|t| t["if"]["properties"]["family"]["const"].as_u64().unwrap())
            .collect();
        families.sort_unstable();
        assert_eq!(families, (1..=14).collect::<Vec<_>>());
    }

    #[test]
    fn recording_rng_is_one_keystream() {
        let key = [7u8; 32];
        let nonce = [9u8; 12];
        let mut r = RecordingRng::new(key, nonce);
        let a = r.draw("a", 16);
        let b = r.draw32("b");
        let mut bare = VectorRng::new(&key, &nonce);
        let whole = bare.draw(48);
        assert_eq!([a.as_slice(), b.as_slice()].concat(), whole);

        let j = r.into_json();
        assert_eq!(j["algorithm"], "chacha20");
        assert_eq!(j["key"].as_str().unwrap(), hex(&key));
        assert_eq!(j["nonce"].as_str().unwrap(), hex(&nonce));
        assert_eq!(
            j["draw_order"],
            json!([{"name": "a", "nbytes": 16}, {"name": "b", "nbytes": 32}])
        );
    }

    #[test]
    #[should_panic(expected = "duplicate draw name")]
    fn duplicate_draw_name_panics() {
        let mut r = RecordingRng::new([0; 32], [0; 12]);
        r.draw("x", 4);
        r.draw("x", 4);
    }

    fn sample(expected: Expected) -> Vector {
        let mut inputs = Map::new();
        inputs.insert("value".into(), json!("00ff"));
        Vector {
            family: 1,
            name: "sample".into(),
            case_kind: "canonical-encode".into(),
            source: "1".into(),
            surfaces: vec!["core".into(), "browser".into()],
            rng: None,
            inputs,
            expected,
        }
    }

    #[test]
    fn emit_and_check_three_shapes() {
        let c = companion();

        let v = sample(Expected::Bytes(vec![0x00, 0xff])).to_json();
        check(&v, &c).unwrap();
        assert_eq!(v["expected"]["bytes"], "00ff");

        let mut w = sample(Expected::Result(json!({
            "per_item": [
                { "item": "c1" },
                { "item": "bad", "outcome": "malformed", "disposition": "reject-permanent" },
            ],
            "converge": true,
        })));
        w.family = 7;
        w.case_kind = "fold".into();
        w.surfaces = vec!["core".into()];
        check(&w.to_json(), &c).unwrap();

        let mut n = sample(Expected::Negative {
            outcome: "depth".into(),
            disposition: "reject-permanent".into(),
        });
        n.case_kind = "canonical-reject".into();
        check(&n.to_json(), &c).unwrap();

        // With a recorded rng block.
        let mut r = RecordingRng::new([1; 32], [2; 12]);
        let _ = r.draw("dek", 32);
        let mut v = sample(Expected::Bytes(vec![]));
        v.rng = Some(r.into_json());
        check(&v.to_json(), &c).unwrap();
    }

    #[test]
    fn file_string_is_deterministic_with_trailing_newline() {
        let a = sample(Expected::Bytes(vec![1, 2])).to_file_string();
        let b = sample(Expected::Bytes(vec![1, 2])).to_file_string();
        assert_eq!(a, b);
        assert!(a.ends_with('\n'));
        assert!(!a.ends_with("\n\n"));
    }

    #[test]
    fn check_rejects_each_violation() {
        let c = companion();
        let base = sample(Expected::Bytes(vec![]));

        // Illegal pair.
        let mut v = base.clone();
        v.expected = Expected::Negative {
            outcome: "malformed".into(),
            disposition: "pending-dependency".into(),
        };
        v.case_kind = "canonical-reject".into();
        assert!(check(&v.to_json(), &c).unwrap_err().contains("legal"));

        // Unknown outcome.
        let mut v = base.clone();
        v.expected = Expected::Negative {
            outcome: "accepted".into(),
            disposition: "reject-permanent".into(),
        };
        v.case_kind = "canonical-reject".into();
        assert!(check(&v.to_json(), &c).is_err());

        // case_kind outside the family vocabulary.
        let mut v = base.clone();
        v.case_kind = "fold".into();
        assert!(check(&v.to_json(), &c).unwrap_err().contains("vocabulary"));

        // sign-then-verify with a browser surface.
        let mut v = base.clone();
        v.family = 3;
        v.case_kind = "sign-then-verify".into();
        assert!(check(&v.to_json(), &c).unwrap_err().contains("browser"));

        // Duplicate surface.
        let mut v = base.clone();
        v.surfaces = vec!["core".into(), "core".into()];
        assert!(check(&v.to_json(), &c)
            .unwrap_err()
            .contains("duplicate surface"));

        // Family out of range.
        let mut v = base.clone();
        v.family = 15;
        assert!(check(&v.to_json(), &c).unwrap_err().contains("range"));

        // Name too long.
        let mut v = base.clone();
        v.name = "x".repeat(121);
        assert!(check(&v.to_json(), &c).unwrap_err().contains("120"));

        // Non-hex expected bytes (raw JSON — unrepresentable via Expected).
        let mut j = base.to_json();
        j["expected"] = json!({ "bytes": "0F" });
        assert!(check(&j, &c).unwrap_err().contains("hex"));

        // Two expected shapes at once.
        let mut j = base.to_json();
        j["expected"] = json!({ "bytes": "00", "result": 1 });
        assert!(check(&j, &c).unwrap_err().contains("exactly one"));

        // Half a pair inside a result row.
        let mut v = base.clone();
        v.family = 7;
        v.case_kind = "fold".into();
        v.expected = Expected::Result(json!({
            "per_item": [{ "item": "a", "outcome": "malformed" }],
            "converge": true,
        }));
        assert!(check(&v.to_json(), &c).unwrap_err().contains("pair"));

        // Duplicate draw names in a raw rng block.
        let mut j = base.to_json();
        j["rng"] = json!({
            "algorithm": "chacha20",
            "key": "00".repeat(32),
            "nonce": "00".repeat(12),
            "draw_order": [
                {"name": "k", "nbytes": 32},
                {"name": "k", "nbytes": 12},
            ],
        });
        assert!(check(&j, &c).unwrap_err().contains("duplicate draw name"));

        // Uppercase rng key hex.
        let mut j = base.to_json();
        j["rng"] = json!({
            "algorithm": "chacha20",
            "key": "0F".repeat(32),
            "nonce": "00".repeat(12),
            "draw_order": [{"name": "k", "nbytes": 1}],
        });
        assert!(check(&j, &c).is_err());
    }
}
