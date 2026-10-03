//! Corpus family 12 (§10.1 service-edge admission) and family 13's
//! byte-framing lanes (§6.2 file framing v2).
//!
//! Family-12 fixture conventions (the companion leaves `context` and
//! `request` free-form; this module defines the closed shape both
//! implementations read — register entries):
//!
//! ```json
//! context: {
//!   "devices":  ["<hex16>", ...],            // enrolled
//!   "revoked":  ["<hex16>", ...],
//!   "sessions": [{ "id", "device", "live": bool }],
//!   "tokens":   [{ "token", "session", "scope": [op...],
//!                  "revoked": bool }],
//!   "quota":    { "<shape>": { "<op>": remaining } }
//!               // per-SHAPE service policy (§10.1); absent = unmetered
//! }
//! request: { "shape": 1..4, "op", "device"?, "session"?, "token"? }
//! ```
//!
//! Check order (multi-fault precedence at the edge — one outcome):
//! identity (device enrolled → revoked?) → session (present → live)
//! → token (present → revoked → scope) → quota. A component that
//! cannot exist for a shape is not part of its rule (§10.1).
//!
//! Family-13 conventions: one `cuts` entry per crash-replay vector;
//! `lock-matrix` outcomes list ONLY the steps that produce an
//! outcome (an unlisted step succeeded); frame payloads are the
//! canonical CBOR of small registered shapes.

use crate::cbor;
use crate::shapes::journal::{
    Kekdestroyed, Outboxmark, Tombstone, FRAME_KEK_DESTROYED, FRAME_OUTBOX_MARK, FRAME_TOMBSTONE,
};
use crate::shapes::ToValue;
use crate::vector::{Expected, Vector};
use serde_json::{json, Map as JsonMap, Value as Json};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// -------------------------------------------------------- family 12

const DEV_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DEV_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const DEV_R: &str = "cccccccccccccccccccccccccccccccc";

/// The shared edge context every family-12 vector reads.
fn edge_context() -> Json {
    json!({
        "devices": [DEV_A, DEV_B],
        "revoked": [DEV_R],
        "sessions": [
            { "id": "s-live", "device": DEV_A, "live": true },
            { "id": "s-ended", "device": DEV_A, "live": false },
        ],
        "tokens": [
            { "token": "t-good", "session": "s-live",
              "scope": ["memory.read", "memory.search"], "revoked": false },
            { "token": "t-revoked", "session": "s-live",
              "scope": ["memory.read"], "revoked": true },
            { "token": "t-orphan", "session": "s-ended",
              "scope": ["memory.read"], "revoked": false },
        ],
        "quota": { "4": { "memory.search": 0 } },
    })
}

fn edge_vector(name: &str, request: Json, expected: Expected) -> Vector {
    let mut inputs = JsonMap::new();
    inputs.insert("context".into(), edge_context());
    inputs.insert("request".into(), request);
    Vector {
        family: 12,
        name: name.into(),
        case_kind: "edge-admission".into(),
        source: "10.1".into(),
        surfaces: vec!["core".into()],
        rng: None,
        inputs,
        expected,
    }
}

fn allowed() -> Expected {
    Expected::Result(json!({ "allowed": true }))
}

fn denied(outcome: &str) -> Expected {
    Expected::Negative {
        outcome: outcome.into(),
        disposition: "edge-deny".into(),
    }
}

fn family12() -> Vec<Vector> {
    vec![
        // ≥ 6 allow paths across the four shapes.
        edge_vector(
            "shape1-enrolled-device-read",
            json!({ "shape": 1, "op": "memory.read", "device": DEV_A }),
            allowed(),
        ),
        edge_vector(
            "shape1-second-device-search",
            json!({ "shape": 1, "op": "memory.search", "device": DEV_B }),
            allowed(),
        ),
        edge_vector(
            "shape2-attested-session",
            json!({ "shape": 2, "op": "memory.read", "device": DEV_A, "session": "s-live" }),
            allowed(),
        ),
        edge_vector(
            "shape3-bearer-read",
            json!({ "shape": 3, "op": "memory.read", "token": "t-good" }),
            allowed(),
        ),
        edge_vector(
            "shape3-bearer-search-in-scope",
            json!({ "shape": 3, "op": "memory.search", "token": "t-good" }),
            allowed(),
        ),
        edge_vector(
            "shape4-mediated-grant-plus-token",
            json!({ "shape": 4, "op": "memory.read", "session": "s-live", "token": "t-good" }),
            allowed(),
        ),
        // One denial per edge outcome this lane owns.
        edge_vector(
            "shape1-unenrolled-no-cert",
            json!({ "shape": 1, "op": "memory.read", "device": "0123456789abcdef0123456789abcdef" }),
            denied("no-cert"),
        ),
        edge_vector(
            "shape1-revoked-device",
            json!({ "shape": 1, "op": "memory.read", "device": DEV_R }),
            denied("cert-revoked"),
        ),
        edge_vector(
            "shape2-missing-session",
            json!({ "shape": 2, "op": "memory.read", "device": DEV_A, "session": "s-nope" }),
            denied("no-session"),
        ),
        edge_vector(
            "shape2-ended-session",
            json!({ "shape": 2, "op": "memory.read", "device": DEV_A, "session": "s-ended" }),
            denied("session-ended"),
        ),
        edge_vector(
            "shape3-missing-token",
            json!({ "shape": 3, "op": "memory.read", "token": "t-nope" }),
            denied("no-token"),
        ),
        edge_vector(
            "shape3-revoked-token",
            json!({ "shape": 3, "op": "memory.read", "token": "t-revoked" }),
            denied("token-revoked"),
        ),
        edge_vector(
            "shape3-token-out-of-scope",
            json!({ "shape": 3, "op": "memory.export", "token": "t-good" }),
            denied("token-scope"),
        ),
        edge_vector(
            "shape3-token-session-ended",
            json!({ "shape": 3, "op": "memory.read", "token": "t-orphan" }),
            denied("session-ended"),
        ),
        edge_vector(
            "shape4-exhausted-quota",
            json!({ "shape": 4, "op": "memory.search", "session": "s-live", "token": "t-good" }),
            denied("quota"),
        ),
    ]
}

// -------------------------------------------------------- family 13

/// CRC32C (Castagnoli, RFC 3720): reflected 0x82F63B78, init and
/// final XOR 0xFFFFFFFF.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0x82F6_3B78 & mask);
        }
    }
    crc ^ 0xFFFF_FFFF
}

const SYNC: &[u8; 4] = b"IPLR";

/// One §6.2 frame: SYNC ‖ len ‖ ~len ‖ type ‖ payload ‖ crc32c.
pub fn frame(frame_type: u8, payload: &[u8]) -> Vec<u8> {
    let len = (1 + payload.len()) as u32;
    let mut body = Vec::with_capacity(9 + payload.len());
    body.extend_from_slice(&len.to_le_bytes());
    body.extend_from_slice(&(!len).to_le_bytes());
    body.push(frame_type);
    body.extend_from_slice(payload);
    let crc = crc32c(&body);
    let mut out = SYNC.to_vec();
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// The §6.2 file header (kind 0 = tenant log).
pub fn file_header(kind: u8, plane_id: &[u8; 32], zone_id: &[u8; 16]) -> Vec<u8> {
    let mut out = b"IPLOG2".to_vec();
    out.push(2);
    out.push(kind);
    out.extend_from_slice(plane_id);
    out.extend_from_slice(zone_id);
    out
}

/// The fixed three-frame stream every framing vector derives from.
fn fixture_stream() -> (Vec<u8>, Vec<Vec<u8>>) {
    let f1 = frame(
        FRAME_TOMBSTONE,
        &cbor::encode(
            &Tombstone {
                item_addr: [0x11; 32],
                erase_op: [0x22; 32],
                target_op: [0x33; 32],
                retired_epoch: 1,
            }
            .to_value(),
        )
        .expect("tombstone encodes"),
    );
    let f2 = frame(
        FRAME_OUTBOX_MARK,
        &cbor::encode(
            &Outboxmark {
                lineage: [0x44; 16],
                gen: 1,
                seq: 7,
            }
            .to_value(),
        )
        .expect("outboxmark encodes"),
    );
    let f3 = frame(
        FRAME_KEK_DESTROYED,
        &cbor::encode(&Kekdestroyed { epoch: 1 }.to_value()).expect("kekdestroyed encodes"),
    );
    let mut stream = file_header(0, &[0xaa; 32], &[0xbb; 16]);
    for f in [&f1, &f2, &f3] {
        stream.extend_from_slice(f);
    }
    (stream, vec![f1, f2, f3])
}

fn family13() -> Vec<Vector> {
    let (stream, frames) = fixture_stream();
    let header_len = 6 + 1 + 1 + 32 + 16; // "IPLOG2" v kind plane zone
    let f1_end = header_len + frames[0].len();
    let f2_end = f1_end + frames[1].len();

    let mut out = Vec::new();

    // frame-roundtrip: the stream parses into exactly these frames
    // and re-serializes byte-identically.
    let mut inputs = JsonMap::new();
    inputs.insert("stream".into(), json!(hex(&stream)));
    inputs.insert(
        "frames".into(),
        json!(frames.iter().map(|f| hex(f)).collect::<Vec<_>>()),
    );
    out.push(Vector {
        family: 13,
        name: "framing-three-frame-roundtrip".into(),
        case_kind: "frame-roundtrip".into(),
        source: "6.2".into(),
        surfaces: vec![
            "browser".into(),
            "storage-macos".into(),
            "storage-linux".into(),
            "storage-windows".into(),
        ],
        rng: None,
        inputs,
        expected: Expected::Bytes(stream.clone()),
    });

    let corruption = |name: &str, offset: usize, bytes: Vec<u8>| {
        let mut inputs = JsonMap::new();
        inputs.insert("stream".into(), json!(hex(&stream)));
        inputs.insert(
            "mutations".into(),
            json!([{ "offset": offset, "bytes": hex(&bytes) }]),
        );
        Vector {
            family: 13,
            name: name.into(),
            case_kind: "corruption-negative".into(),
            source: "6.2".into(),
            surfaces: vec![
                "browser".into(),
                "storage-macos".into(),
                "storage-linux".into(),
                "storage-windows".into(),
            ],
            rng: None,
            inputs,
            expected: Expected::Negative {
                outcome: "log-corrupt".into(),
                disposition: "storage-quarantine".into(),
            },
        }
    };

    // (a) nlen mismatch in frame 2 (len covered twice — caught
    // before seeking): frame layout is SYNC[0..4] len[4..8]
    // nlen[8..12], so +8 is nlen's first byte.
    out.push(corruption(
        "framing-nlen-mismatch-quarantines",
        f1_end + 8,
        vec![stream[f1_end + 8] ^ 0xff],
    ));
    // (b) bad SYNC where frame 3 must start.
    out.push(corruption(
        "framing-bad-sync-quarantines",
        f2_end,
        b"XXXX".to_vec(),
    ));
    // (c) final complete frame, bad CRC — ambiguous, quarantine
    // read-only (never silent truncation).
    out.push(corruption(
        "framing-final-frame-bad-crc-quarantines",
        stream.len() - 1,
        vec![stream[stream.len() - 1] ^ 0x01],
    ));

    // crash-replay: EOF inside the trailing frame truncates to the
    // durable prefix (torn write — never corruption). One cut per
    // vector (fixture convention).
    let crash = |name: &str, cut: usize, truncated_at: usize| {
        let mut inputs = JsonMap::new();
        inputs.insert("stream".into(), json!(hex(&stream)));
        inputs.insert("cuts".into(), json!([cut]));
        Vector {
            family: 13,
            name: name.into(),
            case_kind: "crash-replay".into(),
            source: "6.2".into(),
            surfaces: vec![
                "browser".into(),
                "storage-macos".into(),
                "storage-linux".into(),
                "storage-windows".into(),
            ],
            rng: None,
            inputs,
            expected: Expected::Result(json!({ "truncated_at": truncated_at })),
        }
    };
    // Cut mid-payload of frame 3: prefix = through frame 2.
    out.push(crash("crash-mid-final-frame-truncates", f2_end + 9, f2_end));
    // Cut inside frame 3's SYNC marker: same durable prefix.
    out.push(crash("crash-inside-sync-truncates", f2_end + 2, f2_end));

    // lock-matrix (§6.2 L3): one exclusive advisory lock per plane
    // store; losers get read-only. Outcome rows list ONLY the
    // failing steps.
    let mut inputs = JsonMap::new();
    inputs.insert(
        "script".into(),
        json!([
            { "actor": "A", "action": "acquire", "target": "plane-store" },
            { "actor": "B", "action": "acquire", "target": "plane-store" },
            { "actor": "A", "action": "release", "target": "plane-store" },
            { "actor": "B", "action": "acquire", "target": "plane-store" },
        ]),
    );
    out.push(Vector {
        family: 13,
        name: "lock-exclusive-loser-read-only".into(),
        case_kind: "lock-matrix".into(),
        source: "6.2".into(),
        surfaces: vec![
            "browser".into(),
            "storage-macos".into(),
            "storage-linux".into(),
            "storage-windows".into(),
        ],
        rng: None,
        inputs,
        expected: Expected::Result(json!({
            "outcomes": [
                { "step": 1, "outcome": "lock-denied", "disposition": "edge-deny" },
            ],
        })),
    });

    out
}

/// Families 12 + 13 (edge + framing), family-ordered.
pub fn corpus_edge() -> Vec<Vector> {
    let mut out = family12();
    out.extend(family13());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 3720's own check value.
    #[test]
    fn crc32c_reference() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn committed_edge_corpus_matches_builders() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vectors");
        for v in corpus_edge() {
            let path = dir.join(format!("f{:02}-{}.json", v.family, v.name));
            let committed = std::fs::read_to_string(&path)
                .unwrap_or_else(|_| panic!("{} not minted", path.display()));
            assert_eq!(
                committed,
                v.to_file_string(),
                "{} drifted from its builder",
                v.name
            );
        }
    }

    #[test]
    fn edge_corpus_checks_clean() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let companion: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("d0a-vector-cases.v1.json")).unwrap(),
        )
        .unwrap();
        for v in corpus_edge() {
            crate::vector::check(&v.to_json(), &companion)
                .unwrap_or_else(|e| panic!("{}: {e}", v.name));
        }
    }
}
