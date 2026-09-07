//! The §6/Appendix-A shape layer — typed constructors that mint
//! canonical `cbor::Value` trees for the spec's closed CDDL.
//!
//! Transcription discipline: every production a constructor implements
//! is pinned as a VERBATIM substring of the spec (`CDDL_PINS`, checked
//! against `../owner-plane-d0a-spec.md` by test) — the constructor
//! sits next to the pinned text it must match, and a spec revision
//! that touches a pinned production fails the suite here instead of
//! drifting silently.
//!
//! Writer-side rules carried by this layer (not by the raw encoder):
//! - E1 range: uints ≤ 2^53−1 (browser-safe) — `u()` panics beyond.
//! - E4: optional fields are OMITTED when absent, never null.
//! - E7: set arrays are duplicate-free and SORTED — default key = the
//!   member's full canonical encoding; a declared logical key wins.
//!   `sorted_set_by_key` sorts and PANICS on duplicates: the typed
//!   builders mint only canonical bytes (negative fixtures build raw
//!   `Value`s instead).

use crate::cbor::{self, Value};

pub mod control;
pub mod envelope;
pub mod identity;
pub mod journal;
pub mod memory;

/// Everything the shape layer emits implements this.
pub trait ToValue {
    fn to_value(&self) -> Value;
}

pub type Bytes16 = [u8; 16];
pub type Bytes32 = [u8; 32];

/// E1: uint fields are ≤ 2^53−1 unless a narrower range is stated.
pub const E1_MAX_UINT: u64 = (1 << 53) - 1;

/// Range-guarded uint (E1). Panics on out-of-range — a minting bug,
/// not a validation outcome.
pub(crate) fn u(n: u64) -> Value {
    assert!(n <= E1_MAX_UINT, "uint {n} exceeds the E1 2^53-1 range");
    Value::Uint(n)
}

pub(crate) fn bytes(b: &[u8]) -> Value {
    Value::Bytes(b.to_vec())
}

pub(crate) fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

/// The canonical-encoding sort key of one or more key components,
/// concatenated (fixed-width byte strings and shortest-form uints both
/// order correctly under bytewise comparison of their encodings).
pub(crate) fn key_bytes(parts: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        out.extend(cbor::encode(p).expect("key component encodes"));
    }
    out
}

/// E7 set builder: sort members by key bytes, panic on a duplicate
/// key (two members sharing a key are non-canonical even when
/// byte-distinct).
pub(crate) fn sorted_set_by_key(mut items: Vec<(Vec<u8>, Value)>, what: &str) -> Vec<Value> {
    items.sort_by(|a, b| a.0.cmp(&b.0));
    for w in items.windows(2) {
        assert!(
            w[0].0 != w[1].0,
            "duplicate {what} set key (E7: non-canonical)"
        );
    }
    items.into_iter().map(|(_, v)| v).collect()
}

/// E7 default-key set: sorted by the member's full canonical encoding.
pub(crate) fn sorted_set_default(members: Vec<Value>, what: &str) -> Vec<Value> {
    let items = members
        .into_iter()
        .map(|v| (cbor::encode(&v).expect("member encodes"), v))
        .collect();
    sorted_set_by_key(items, what)
}

// ---- closed vocabularies (Appendix A primitives) ----

macro_rules! closed_vocab {
    ($(#[$doc:meta])* $name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name {
            $($variant),+
        }
        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];
            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $wire),+
                }
            }
        }
        impl ToValue for $name {
            fn to_value(&self) -> Value {
                Value::Text(self.as_str().to_string())
            }
        }
    };
}
pub(crate) use closed_vocab;

closed_vocab!(Sigalg { Ed25519 => "ed25519", P256 => "p256" });

closed_vocab!(Class {
    Public => "public",
    Internal => "internal",
    Private => "private",
    Sensitive => "sensitive",
});

closed_vocab!(Kind {
    Observation => "observation",
    Decision => "decision",
    Episode => "episode",
    Procedure => "procedure",
    Preference => "preference",
});

closed_vocab!(Devclass {
    HostedBrowser => "hosted-browser",
    OwnerBrowser => "owner-browser",
    NativeApp => "native-app",
    Daemon => "daemon",
    MobileAttested => "mobile-attested",
    Mobile => "mobile",
    Other => "other",
});

closed_vocab!(Spaceclass {
    Workflow => "workflow",
    Personal => "personal",
    Project => "project",
    Audit => "audit",
});

closed_vocab!(Verb {
    Search => "search",
    Read => "read",
    EvidenceRead => "evidence.read",
    Propose => "propose",
    Assert => "assert",
    JudgeSafe => "judge.safe",
    JudgeFull => "judge.full",
    PinSafe => "pin.safe",
    PinFull => "pin.full",
    EraseRequest => "erase.request",
    Raise => "raise",
    Declassify => "declassify",
    Export => "export",
    Import => "import",
    CurateInstruction => "curate.instruction",
    AuditWrite => "audit.write",
    Admin => "admin",
});

closed_vocab!(Strictness { Strict => "strict", Lenient => "lenient" });

closed_vocab!(DeadlineFallback {
    FailClosed => "fail-closed",
    Budgets => "budgets",
});

/// `hlc = [ms, uint]`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hlc {
    pub ms: u64,
    pub count: u64,
}

impl ToValue for Hlc {
    fn to_value(&self) -> Value {
        Value::Array(vec![u(self.ms), u(self.count)])
    }
}

// ---- common record shapes ----

/// `head = { lineage: bytes16, gen: uint, seq: uint, op: bytes32 }`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Head {
    pub lineage: Bytes16,
    pub gen: u64,
    pub seq: u64,
    pub op: Bytes32,
}

impl ToValue for Head {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("lineage", bytes(&self.lineage)),
            ("gen", u(self.gen)),
            ("seq", u(self.seq)),
            ("op", bytes(&self.op)),
        ])
    }
}

/// `fencecoord = { lineage: bytes16, gen: uint, seq: uint }`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fencecoord {
    pub lineage: Bytes16,
    pub gen: u64,
    pub seq: u64,
}

impl ToValue for Fencecoord {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("lineage", bytes(&self.lineage)),
            ("gen", u(self.gen)),
            ("seq", u(self.seq)),
        ])
    }
}

/// `issuerid = { src: "device", cert: bytes32 }
///            / { src: "service", key_id: bytes32 }`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Issuerid {
    Device { cert: Bytes32 },
    Service { key_id: Bytes32 },
}

impl ToValue for Issuerid {
    fn to_value(&self) -> Value {
        match self {
            Issuerid::Device { cert } => {
                cbor::map(vec![("src", text("device")), ("cert", bytes(cert))])
            }
            Issuerid::Service { key_id } => {
                cbor::map(vec![("src", text("service")), ("key_id", bytes(key_id))])
            }
        }
    }
}

/// `factref = { kind: "op", ref: bytes32 } / { kind: "stmt", ref: bytes32 }`
/// — invalidations cite either domain (D-179).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Factref {
    Op(Bytes32),
    Stmt(Bytes32),
}

impl ToValue for Factref {
    fn to_value(&self) -> Value {
        let (kind, r) = match self {
            Factref::Op(r) => ("op", r),
            Factref::Stmt(r) => ("stmt", r),
        };
        cbor::map(vec![("kind", text(kind)), ("ref", bytes(r))])
    }
}

/// `opfactref = { kind: "op", ref: bytes32 }` — terminal bases are
/// op-kind only (D-185/D-193).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opfactref(pub Bytes32);

impl ToValue for Opfactref {
    fn to_value(&self) -> Value {
        cbor::map(vec![("kind", text("op")), ("ref", bytes(&self.0))])
    }
}

/// `frontierclose = { zone_id: ulid, lineage: bytes16,
///   heads: [* head] }` — heads: set keyed by `gen`, ≤ 65 (E8/D-152);
/// total-override semantics for the ended authority (D-135/D-143).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontierclose {
    pub zone_id: Bytes16,
    pub lineage: Bytes16,
    pub heads: Vec<Head>,
}

impl ToValue for Frontierclose {
    fn to_value(&self) -> Value {
        let heads = sorted_set_by_key(
            self.heads
                .iter()
                .map(|h| (key_bytes(&[u(h.gen)]), h.to_value()))
                .collect(),
            "frontierclose.heads (key gen)",
        );
        cbor::map(vec![
            ("zone_id", bytes(&self.zone_id)),
            ("lineage", bytes(&self.lineage)),
            ("heads", Value::Array(heads)),
        ])
    }
}

/// `ratifycutoff = { zone_id: ulid, lineage: bytes16, gen: uint,
///   accepted_through: head }` — logical key (zone_id, lineage, gen)
/// (D-120); folds per D-135 (first = Bounded, later = max).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ratifycutoff {
    pub zone_id: Bytes16,
    pub lineage: Bytes16,
    pub gen: u64,
    pub accepted_through: Head,
}

impl ToValue for Ratifycutoff {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("zone_id", bytes(&self.zone_id)),
            ("lineage", bytes(&self.lineage)),
            ("gen", u(self.gen)),
            ("accepted_through", self.accepted_through.to_value()),
        ])
    }
}

/// `zoneheads = { zone_id: ulid, heads: [* head] }` — heads: set
/// keyed by `gen` (at most one live head per generation, D-113), ≤ 65.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zoneheads {
    pub zone_id: Bytes16,
    pub heads: Vec<Head>,
}

impl ToValue for Zoneheads {
    fn to_value(&self) -> Value {
        let heads = sorted_set_by_key(
            self.heads
                .iter()
                .map(|h| (key_bytes(&[u(h.gen)]), h.to_value()))
                .collect(),
            "zoneheads.heads (key gen)",
        );
        cbor::map(vec![
            ("zone_id", bytes(&self.zone_id)),
            ("heads", Value::Array(heads)),
        ])
    }
}

/// `kekwrap = { v: 1, plane_id: bytes32, zone_id: ulid, epoch: uint,
///   recipient_device: bytes16, recipient_kem_key: bytes32,
///   kem: "hpke-p256-v1", enc: bstr .size 65, ct: bstr .size 48 }`
/// — recipient_kem_key = the key_id of the recipient's EFFECTIVE
/// certificate's KEM key (global equality, D-116/D-125).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kekwrap {
    pub plane_id: Bytes32,
    pub zone_id: Bytes16,
    pub epoch: u64,
    pub recipient_device: Bytes16,
    pub recipient_kem_key: Bytes32,
    pub enc: [u8; 65],
    pub ct: [u8; 48],
}

impl ToValue for Kekwrap {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("v", u(1)),
            ("plane_id", bytes(&self.plane_id)),
            ("zone_id", bytes(&self.zone_id)),
            ("epoch", u(self.epoch)),
            ("recipient_device", bytes(&self.recipient_device)),
            ("recipient_kem_key", bytes(&self.recipient_kem_key)),
            ("kem", text("hpke-p256-v1")),
            ("enc", bytes(&self.enc)),
            ("ct", bytes(&self.ct)),
        ])
    }
}

/// The kekwrap E7 logical key: `(zone_id, epoch, recipient_device)`.
impl Kekwrap {
    pub(crate) fn set_key(&self) -> Vec<u8> {
        key_bytes(&[
            bytes(&self.zone_id),
            u(self.epoch),
            bytes(&self.recipient_device),
        ])
    }
}

/// `lineagedef = { lineage: bytes16, device_id: bytes16, max_generations: uint }`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lineagedef {
    pub lineage: Bytes16,
    pub device_id: Bytes16,
    pub max_generations: u64,
}

impl ToValue for Lineagedef {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("lineage", bytes(&self.lineage)),
            ("device_id", bytes(&self.device_id)),
            ("max_generations", u(self.max_generations)),
        ])
    }
}

/// `polref = { id: text, version: uint, hash: bytes32 }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Polref {
    pub id: String,
    pub version: u64,
    pub hash: Bytes32,
}

impl ToValue for Polref {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("id", text(&self.id)),
            ("version", u(self.version)),
            ("hash", bytes(&self.hash)),
        ])
    }
}

/// `spacedef = { space_id: ulid, zone_id: ulid, name_hash: bytes32,
///   space_class: spaceclass, class_minimum: class, status_policy: polref }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spacedef {
    pub space_id: Bytes16,
    pub zone_id: Bytes16,
    pub name_hash: Bytes32,
    pub space_class: Spaceclass,
    pub class_minimum: Class,
    pub status_policy: Polref,
}

impl ToValue for Spacedef {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("space_id", bytes(&self.space_id)),
            ("zone_id", bytes(&self.zone_id)),
            ("name_hash", bytes(&self.name_hash)),
            ("space_class", self.space_class.to_value()),
            ("class_minimum", self.class_minimum.to_value()),
            ("status_policy", self.status_policy.to_value()),
        ])
    }
}

/// One `zonepolicy.time_witnesses` entry: `bytes16 / "connect"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeWitness {
    Device(Bytes16),
    Connect,
}

impl ToValue for TimeWitness {
    fn to_value(&self) -> Value {
        match self {
            TimeWitness::Device(d) => bytes(d),
            TimeWitness::Connect => text("connect"),
        }
    }
}

/// `zonepolicy = { v: 1, zone_id: ulid, strictness: "strict" / "lenient",
///   deadline_fallback: "fail-closed" / "budgets",
///   require_cert_deadlines: bool, ? grant_epoch_slack: uint,
///   ? time_witnesses: [+ bytes16 / "connect"], ? connect_service_key: bytes32 }`
/// — deadline_fallback "fail-closed" REQUIRES require_cert_deadlines
/// (cross-field, D-76); connect_service_key REQUIRED iff "connect" is
/// a witness (D-70). Cross-field validity is the FOLD's job — the
/// builder emits what it is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zonepolicy {
    pub zone_id: Bytes16,
    pub strictness: Strictness,
    pub deadline_fallback: DeadlineFallback,
    pub require_cert_deadlines: bool,
    pub grant_epoch_slack: Option<u64>,
    pub time_witnesses: Option<Vec<TimeWitness>>,
    pub connect_service_key: Option<Bytes32>,
}

impl ToValue for Zonepolicy {
    fn to_value(&self) -> Value {
        let mut entries = vec![
            ("v", u(1)),
            ("zone_id", bytes(&self.zone_id)),
            ("strictness", self.strictness.to_value()),
            ("deadline_fallback", self.deadline_fallback.to_value()),
            (
                "require_cert_deadlines",
                Value::Bool(self.require_cert_deadlines),
            ),
        ];
        if let Some(s) = self.grant_epoch_slack {
            entries.push(("grant_epoch_slack", u(s)));
        }
        if let Some(ws) = &self.time_witnesses {
            let members = sorted_set_default(
                ws.iter().map(|w| w.to_value()).collect(),
                "zonepolicy.time_witnesses",
            );
            entries.push(("time_witnesses", Value::Array(members)));
        }
        if let Some(k) = &self.connect_service_key {
            entries.push(("connect_service_key", bytes(k)));
        }
        cbor::map(entries)
    }
}

/// `erasemref = { item_addr: bytes32, erase_op: bytes32,
///   target_op: bytes32 }` — logical key `item_addr` (D-66).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Erasemref {
    pub item_addr: Bytes32,
    pub erase_op: Bytes32,
    pub target_op: Bytes32,
}

impl ToValue for Erasemref {
    fn to_value(&self) -> Value {
        cbor::map(vec![
            ("item_addr", bytes(&self.item_addr)),
            ("erase_op", bytes(&self.erase_op)),
            ("target_op", bytes(&self.target_op)),
        ])
    }
}

// ---- the verbatim CDDL pins (checked against the spec by test) ----

#[cfg(test)]
pub(crate) const CDDL_PINS_COMMON: &[&str] = &[
    r#"bytes16 = bstr .size 16    bytes32 = bstr .size 32
ulid = bytes16             ms = uint
hlc = [ms, uint]
sigalg = "ed25519" / "p256"
class = "public" / "internal" / "private" / "sensitive"
kind = "observation" / "decision" / "episode" / "procedure" / "preference"
devclass = "hosted-browser" / "owner-browser" / "native-app" / "daemon"
         / "mobile-attested" / "mobile" / "other"
spaceclass = "workflow" / "personal" / "project" / "audit"
verb = "search" / "read" / "evidence.read" / "propose" / "assert"
     / "judge.safe" / "judge.full" / "pin.safe" / "pin.full"
     / "erase.request" / "raise" / "declassify" / "export" / "import"
     / "curate.instruction" / "audit.write" / "admin""#,
    r#"fencecoord = { lineage: bytes16, gen: uint, seq: uint }
head = { lineage: bytes16, gen: uint, seq: uint, op: bytes32 }
issuerid = { src: "device", cert: bytes32 }
         / { src: "service", key_id: bytes32 }"#,
    r#"kekwrap = { v: 1, plane_id: bytes32, zone_id: ulid, epoch: uint,
  recipient_device: bytes16, recipient_kem_key: bytes32,
  kem: "hpke-p256-v1", enc: bstr .size 65, ct: bstr .size 48 }"#,
    r#"lineagedef = { lineage: bytes16, device_id: bytes16, max_generations: uint }
spacedef = { space_id: ulid, zone_id: ulid, name_hash: bytes32,
  space_class: spaceclass, class_minimum: class, status_policy: polref }
zonepolicy = { v: 1, zone_id: ulid, strictness: "strict" / "lenient",
  deadline_fallback: "fail-closed" / "budgets",
  require_cert_deadlines: bool, ? grant_epoch_slack: uint,
  ? time_witnesses: [+ bytes16 / "connect"],   ; set (E7), <= 64 (D-96);
                                               ;   stable device_ids
  ? connect_service_key: bytes32 }"#,
    r#"erasemref = { item_addr: bytes32, erase_op: bytes32,
  target_op: bytes32 }"#,
    r#"frontierclose = { zone_id: ulid, lineage: bytes16,
  heads: [* head] }"#,
    r#"ratifycutoff = { zone_id: ulid, lineage: bytes16, gen: uint,
  accepted_through: head }"#,
    r#"zoneheads = { zone_id: ulid, heads: [* head] }"#,
    r#"factref = { kind: "op", ref: bytes32 }
        / { kind: "stmt", ref: bytes32 }"#,
    r#"opfactref = { kind: "op", ref: bytes32 }"#,
    r#"polref = { id: text, version: uint, hash: bytes32 }"#,
];

#[cfg(test)]
pub(crate) fn spec_text() -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("owner-plane-d0a-spec.md"),
    )
    .expect("spec readable next to the core crate")
}

#[cfg(test)]
pub(crate) fn assert_pins(pins: &[&str]) {
    let spec = spec_text();
    for pin in pins {
        assert!(
            spec.contains(pin),
            "CDDL pin drifted from the spec:\n---\n{pin}\n---"
        );
    }
}

/// Test helper: the text keys of a map `Value`, in insertion order.
#[cfg(test)]
pub(crate) fn map_keys(v: &Value) -> Vec<String> {
    match v {
        Value::Map(entries) => entries
            .iter()
            .map(|(k, _)| match k {
                Value::Text(s) => s.clone(),
                other => panic!("non-text map key: {other:?}"),
            })
            .collect(),
        other => panic!("not a map: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cddl_pins_are_verbatim() {
        assert_pins(CDDL_PINS_COMMON);
    }

    #[test]
    fn vocab_sizes_are_closed() {
        assert_eq!(Sigalg::ALL.len(), 2);
        assert_eq!(Class::ALL.len(), 4);
        assert_eq!(Kind::ALL.len(), 5);
        assert_eq!(Devclass::ALL.len(), 7);
        assert_eq!(Spaceclass::ALL.len(), 4);
        assert_eq!(Verb::ALL.len(), 17);
    }

    #[test]
    fn e1_uint_range_guard() {
        assert_eq!(u(E1_MAX_UINT), Value::Uint(E1_MAX_UINT));
        assert!(std::panic::catch_unwind(|| u(E1_MAX_UINT + 1)).is_err());
    }

    #[test]
    fn hlc_is_a_two_element_array() {
        let v = Hlc { ms: 5, count: 2 }.to_value();
        assert_eq!(v, Value::Array(vec![Value::Uint(5), Value::Uint(2)]));
    }

    #[test]
    fn record_field_sets() {
        let head = Head {
            lineage: [1; 16],
            gen: 2,
            seq: 3,
            op: [4; 32],
        };
        assert_eq!(map_keys(&head.to_value()), ["lineage", "gen", "seq", "op"]);
        let fc = Fencecoord {
            lineage: [1; 16],
            gen: 2,
            seq: 3,
        };
        assert_eq!(map_keys(&fc.to_value()), ["lineage", "gen", "seq"]);
        let rc = Ratifycutoff {
            zone_id: [9; 16],
            lineage: [1; 16],
            gen: 2,
            accepted_through: head,
        };
        assert_eq!(
            map_keys(&rc.to_value()),
            ["zone_id", "lineage", "gen", "accepted_through"]
        );
        let lin = Lineagedef {
            lineage: [1; 16],
            device_id: [2; 16],
            max_generations: 8,
        };
        assert_eq!(
            map_keys(&lin.to_value()),
            ["lineage", "device_id", "max_generations"]
        );
        let em = Erasemref {
            item_addr: [1; 32],
            erase_op: [2; 32],
            target_op: [3; 32],
        };
        assert_eq!(
            map_keys(&em.to_value()),
            ["item_addr", "erase_op", "target_op"]
        );
    }

    #[test]
    fn issuerid_and_factref_arms() {
        let d = Issuerid::Device { cert: [7; 32] }.to_value();
        assert_eq!(map_keys(&d), ["src", "cert"]);
        let s = Issuerid::Service { key_id: [8; 32] }.to_value();
        assert_eq!(map_keys(&s), ["src", "key_id"]);
        let f = Factref::Stmt([9; 32]).to_value();
        assert_eq!(map_keys(&f), ["kind", "ref"]);
        // opfactref is structurally the op arm of factref.
        assert_eq!(
            Opfactref([9; 32]).to_value(),
            Factref::Op([9; 32]).to_value()
        );
    }

    #[test]
    fn frontierclose_sorts_heads_by_gen() {
        let h = |gen: u64| Head {
            lineage: [1; 16],
            gen,
            seq: 1,
            op: [gen as u8; 32],
        };
        let fc = Frontierclose {
            zone_id: [2; 16],
            lineage: [1; 16],
            heads: vec![h(300), h(1), h(24)],
        };
        let v = fc.to_value();
        let Value::Map(entries) = &v else { panic!() };
        let heads = &entries
            .iter()
            .find(|(k, _)| *k == Value::Text("heads".into()))
            .unwrap()
            .1;
        let Value::Array(items) = heads else { panic!() };
        let gens: Vec<u64> = items
            .iter()
            .map(|m| {
                let Value::Map(fields) = m else { panic!() };
                fields
                    .iter()
                    .find_map(|(k, val)| {
                        (*k == Value::Text("gen".into())).then(|| match val {
                            Value::Uint(g) => *g,
                            _ => panic!(),
                        })
                    })
                    .unwrap()
            })
            .collect();
        // 1 < 24 < 300 crosses shortest-form header widths (0x01,
        // 0x1818, 0x19012c) — encoded-byte order must equal numeric.
        assert_eq!(gens, [1, 24, 300]);
    }

    #[test]
    #[should_panic(expected = "duplicate")]
    fn duplicate_set_keys_panic() {
        let h = |seq: u64| Head {
            lineage: [1; 16],
            gen: 7,
            seq,
            op: [0; 32],
        };
        // Same gen, different bytes: non-canonical under the keyed set.
        Zoneheads {
            zone_id: [2; 16],
            heads: vec![h(1), h(2)],
        }
        .to_value();
    }

    #[test]
    fn kekwrap_literals_and_key() {
        let w = Kekwrap {
            plane_id: [1; 32],
            zone_id: [2; 16],
            epoch: 3,
            recipient_device: [4; 16],
            recipient_kem_key: [5; 32],
            enc: [6; 65],
            ct: [7; 48],
        };
        let v = w.to_value();
        assert_eq!(
            map_keys(&v),
            [
                "v",
                "plane_id",
                "zone_id",
                "epoch",
                "recipient_device",
                "recipient_kem_key",
                "kem",
                "enc",
                "ct"
            ]
        );
        let Value::Map(entries) = &v else { panic!() };
        assert!(entries.contains(&(Value::Text("v".into()), Value::Uint(1))));
        assert!(entries.contains(&(
            Value::Text("kem".into()),
            Value::Text("hpke-p256-v1".into())
        )));
        // The E7 logical key orders by (zone_id, epoch, recipient_device).
        let mut w2 = w.clone();
        w2.epoch = 2;
        assert!(w2.set_key() < w.set_key());
    }

    #[test]
    fn zonepolicy_optionals_and_witness_sort() {
        let base = Zonepolicy {
            zone_id: [1; 16],
            strictness: Strictness::Strict,
            deadline_fallback: DeadlineFallback::FailClosed,
            require_cert_deadlines: true,
            grant_epoch_slack: None,
            time_witnesses: None,
            connect_service_key: None,
        };
        assert_eq!(
            map_keys(&base.to_value()),
            [
                "v",
                "zone_id",
                "strictness",
                "deadline_fallback",
                "require_cert_deadlines"
            ]
        );
        let full = Zonepolicy {
            grant_epoch_slack: Some(2),
            time_witnesses: Some(vec![
                TimeWitness::Connect,
                TimeWitness::Device([9; 16]),
                TimeWitness::Device([3; 16]),
            ]),
            connect_service_key: Some([8; 32]),
            ..base
        };
        let v = full.to_value();
        assert_eq!(
            map_keys(&v),
            [
                "v",
                "zone_id",
                "strictness",
                "deadline_fallback",
                "require_cert_deadlines",
                "grant_epoch_slack",
                "time_witnesses",
                "connect_service_key"
            ]
        );
        // Default E7 key = full canonical encoding: 16-byte bstrs
        // (header 0x50) sort before the text "connect" (header 0x67).
        let Value::Map(entries) = &v else { panic!() };
        let ws = &entries
            .iter()
            .find(|(k, _)| *k == Value::Text("time_witnesses".into()))
            .unwrap()
            .1;
        let Value::Array(items) = ws else { panic!() };
        assert_eq!(
            items.as_slice(),
            [
                Value::Bytes(vec![3; 16]),
                Value::Bytes(vec![9; 16]),
                Value::Text("connect".into()),
            ]
        );
    }

    #[test]
    fn spacedef_and_polref_field_sets() {
        let sd = Spacedef {
            space_id: [1; 16],
            zone_id: [2; 16],
            name_hash: [3; 32],
            space_class: Spaceclass::Personal,
            class_minimum: Class::Private,
            status_policy: Polref {
                id: "status-v1".into(),
                version: 1,
                hash: [4; 32],
            },
        };
        assert_eq!(
            map_keys(&sd.to_value()),
            [
                "space_id",
                "zone_id",
                "name_hash",
                "space_class",
                "class_minimum",
                "status_policy"
            ]
        );
    }
}
