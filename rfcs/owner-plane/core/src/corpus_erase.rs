//! Corpus family 13, the §5.5 erase-crash matrix: the six-state
//! rotation/erase machine replayed over a tenant zone log with crash
//! cuts — one vector per recorded state, plus the
//! survivor-completeness negative ("omission of any survivor blocks
//! destruction") and the rotation-queue serialization negative (an
//! N+1 Fence before N's tombstones — D-89). The state-4 vector is
//! D-73's third-rotation case: its survivor set contains an
//! epoch-1-committed item (wrapper-current — original commit epoch
//! irrelevant after rewrap), and a post-Fence commit stays outside
//! the set by construction (D-67).
//!
//! Lane conventions (fixture-defined; the reducer mirrors them):
//! - `inputs.stream` = ONE tenant zone log (§6.2 framing, kind 0);
//!   `cuts` = crash byte offsets — the durable prefix truncates to
//!   the last complete frame (L1); `machine_state` = the §5.5 state
//!   recovery re-enters at EVERY cut, derived from the CONFORMANT
//!   durable prefix (frames before the first violation).
//! - The manifest oracle: §5.5 state 6 re-derives tombstones from
//!   the rotation's typed `erase_manifest`, which lives in the
//!   CONTROL op — a tenant-log-only lane cannot carry it. The FULL
//!   stream's tombstone set (rotation keyed by
//!   `retired_epoch = new_epoch − 1`) is the same typed entries in
//!   durable form and stands in for the control-side read.
//! - `fence_frontier`/`control_frontier`/`recipients_hash` are
//!   opaque commitments here: mirror-checked (Fence ↔ RewrapDone,
//!   D-97/D-106) and probe-recovered; deriving them is fold
//!   territory.
//! - `wrap_hash` = plain (untagged) SHA-256 of canonical ItemWrap
//!   bytes (§5.5's own definition).
//! - A violation quarantines the store read-only (§6.2 recovery):
//!   replay stops at the first violation; `outcomes` rows list
//!   violations in stream order — all `(log-corrupt,
//!   storage-quarantine)`, the §6.2/§10.5 storage convention.
//! - Probes hold at every cut (each vector's cuts share one durable
//!   prefix). Probe values are canonical CBOR of the named construct
//!   (register #17): `serving.epoch` = the I3 served epoch (last
//!   durable Fence's `kek_epoch`; 1 with no Fence, D-92);
//!   `fence.recovered` = the last Fence payload; `rewrap.recovered`
//!   = the last durable ItemRewrap's `wrap` map;
//!   `survivorset.recomputed` = the verified rotation's canonical
//!   survivorset; `tombstones.rederived`/`tombstones.durable` =
//!   arrays of tombstone payload maps in `item_addr` order.

use crate::cbor::{self, Value};
use crate::corpus_edge::{file_header, frame};
use crate::keyschedule::{item_addr, seal_item, wrap_dek};
use crate::shapes::journal::{
    Fenceframe, Itemcommit, Itemrewrapframe, Itemwrap, Kekdestroyed, Rewrapdone, Survivorset,
    Tombstone, FRAME_FENCE, FRAME_ITEM_COMMIT, FRAME_ITEM_REWRAP, FRAME_KEK_DESTROYED,
    FRAME_REWRAP_DONE, FRAME_TOMBSTONE,
};
use crate::shapes::ToValue;
use crate::vector::{Expected, Vector};
use serde_json::{json, Map as JsonMap};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ops::Range;

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

const PLANE: [u8; 32] = [0x0d; 32];
const ZONE: [u8; 16] = [0x0e; 16];
const LINEAGE: [u8; 16] = [0x44; 16];
const ERASE_OP: [u8; 32] = [0xf1; 32];
const TARGET_OP: [u8; 32] = [0xf2; 32];

/// Per-epoch KAT KEK (the storage machine never derives KEKs).
fn kek(epoch: u64) -> [u8; 32] {
    [0x60 + epoch as u8; 32]
}

/// Plain SHA-256 of canonical ItemWrap bytes — §5.5's `wrap_hash`.
fn wrap_hash(wrap: &Itemwrap) -> [u8; 32] {
    let bytes = cbor::encode(&wrap.to_value()).expect("itemwrap encodes");
    Sha256::digest(&bytes).into()
}

/// A tenant zone log under construction. Tracks each item's current
/// wrap (the reducer rebuilds the same registry from the frames).
struct Log {
    stream: Vec<u8>,
    seq: u64,
    wraps: BTreeMap<[u8; 32], Itemwrap>,
}

impl Log {
    fn new() -> Self {
        Log {
            stream: file_header(0, &PLANE, &ZONE),
            seq: 0,
            wraps: BTreeMap::new(),
        }
    }

    fn push(&mut self, frame_type: u8, payload: &Value) -> Range<usize> {
        let start = self.stream.len();
        let bytes = frame(
            frame_type,
            &cbor::encode(payload).expect("frame payload encodes"),
        );
        self.stream.extend_from_slice(&bytes);
        start..self.stream.len()
    }

    /// Commit item `i` under `epoch`: real AEAD seal + real DEK wrap.
    fn commit(&mut self, i: u8, epoch: u64) -> [u8; 32] {
        let dek = [0x90 + i; 32];
        let nonce = [0xa0 + i; 12];
        let plaintext = format!("erase-crash item {i}");
        let core = seal_item(&dek, nonce, &PLANE, &ZONE, plaintext.as_bytes());
        let addr = item_addr(&core);
        let wrap = Itemwrap {
            item_addr: addr,
            key_wrap_epoch: epoch,
            wrapped_dek: wrap_dek(&kek(epoch), &PLANE, &ZONE, epoch, &addr, &dek),
        };
        self.seq += 1;
        let commit = Itemcommit {
            core,
            wrap,
            lineage: LINEAGE,
            gen: 1,
            seq: self.seq,
        };
        self.push(FRAME_ITEM_COMMIT, &commit.to_value());
        self.wraps.insert(addr, wrap);
        addr
    }

    /// The rotation-`r` Fence to `new_epoch` (opaque commitments).
    fn fence(&mut self, r: u8, new_epoch: u64) -> (Fenceframe, Range<usize>) {
        let f = Fenceframe {
            kek_epoch: new_epoch,
            rotation_op: [0xb0 + r; 32],
            fence_frontier: [0xc0 + r; 32],
            control_frontier: [0xd0 + r; 32],
            recipients_hash: [0xe0 + r; 32],
        };
        let range = self.push(FRAME_FENCE, &f.to_value());
        (f, range)
    }

    /// Rewrap `addr`'s DEK under `epoch` (idempotent per I2). The
    /// wrapped DEK re-derives from the item's own DEK, which the
    /// deterministic constants reproduce.
    fn rewrap(&mut self, i: u8, addr: [u8; 32], epoch: u64) -> (Itemwrap, Range<usize>) {
        let dek = [0x90 + i; 32];
        let wrap = Itemwrap {
            item_addr: addr,
            key_wrap_epoch: epoch,
            wrapped_dek: wrap_dek(&kek(epoch), &PLANE, &ZONE, epoch, &addr, &dek),
        };
        let range = self.push(FRAME_ITEM_REWRAP, &Itemrewrapframe { wrap }.to_value());
        self.wraps.insert(addr, wrap);
        (wrap, range)
    }

    /// The state-4 record over exactly `members` (their CURRENT —
    /// post-rewrap — wraps). The negative vector passes an
    /// incomplete member list deliberately.
    fn rewrap_done(&mut self, f: &Fenceframe, members: &[[u8; 32]]) -> (Survivorset, Range<usize>) {
        let set = Survivorset {
            pairs: members
                .iter()
                .map(|addr| (*addr, wrap_hash(&self.wraps[addr])))
                .collect(),
        };
        let done = Rewrapdone {
            kek_epoch: f.kek_epoch,
            rotation_op: f.rotation_op,
            count: members.len() as u64,
            fence_frontier: f.fence_frontier,
            control_frontier: f.control_frontier,
            recipients_hash: f.recipients_hash,
            survivors: set.hash(),
        };
        let range = self.push(FRAME_REWRAP_DONE, &done.to_value());
        (set, range)
    }

    fn kek_destroyed(&mut self, retiring: u64) -> Range<usize> {
        self.push(
            FRAME_KEK_DESTROYED,
            &Kekdestroyed { epoch: retiring }.to_value(),
        )
    }

    fn tombstone(&mut self, addr: [u8; 32], retired_epoch: u64) -> (Tombstone, Range<usize>) {
        let t = Tombstone {
            item_addr: addr,
            erase_op: ERASE_OP,
            target_op: TARGET_OP,
            retired_epoch,
        };
        let range = self.push(FRAME_TOMBSTONE, &t.to_value());
        (t, range)
    }
}

fn probe(name: &str, value: &Value) -> serde_json::Value {
    json!({ "name": name, "value": hex(&cbor::encode(value).expect("probe encodes")) })
}

fn ec_vector(
    name: &str,
    stream: &[u8],
    cuts: Vec<usize>,
    machine_state: u64,
    result: serde_json::Value,
) -> Vector {
    let mut inputs = JsonMap::new();
    inputs.insert("stream".into(), json!(hex(stream)));
    inputs.insert("cuts".into(), json!(cuts));
    inputs.insert("machine_state".into(), json!(machine_state));
    Vector {
        family: 13,
        name: name.into(),
        case_kind: "erase-crash-matrix".into(),
        source: "5.5".into(),
        surfaces: vec![
            "browser".into(),
            "storage-macos".into(),
            "storage-linux".into(),
            "storage-windows".into(),
        ],
        rng: None,
        inputs,
        expected: Expected::Result(result),
    }
}

/// The shared erase-rotation log (states 5 and 6 cut it at different
/// points): a and b at epoch 1; the rotation to epoch 2 erases b.
fn erase_log() -> (Log, usize, (Tombstone, Range<usize>)) {
    let mut log = Log::new();
    let a = log.commit(1, 1);
    let b = log.commit(2, 1);
    let (f, _) = log.fence(1, 2);
    log.rewrap(1, a, 2);
    log.rewrap_done(&f, &[a]);
    let kd = log.kek_destroyed(1);
    let kd_end = kd.end;
    let t = log.tombstone(b, 1);
    (log, kd_end, t)
}

pub fn corpus_erase() -> Vec<Vector> {
    let mut out = Vec::new();

    // State 1: the rotation is control-accepted; its Fence is torn by
    // the crash — recovery finds no durable activation and re-enters
    // at 1 (two cuts inside the Fence frame truncate identically).
    {
        let mut log = Log::new();
        log.commit(1, 1);
        let (_, fr) = log.fence(1, 2);
        out.push(ec_vector(
            "erase-crash-state1-accepted-unfenced",
            &log.stream,
            vec![fr.start + 5, fr.end - 3],
            1,
            json!({ "state_probes": [probe("serving.epoch", &Value::Uint(1))] }),
        ));
    }

    // State 2 (D-97): crash right after the Fence — the committed
    // intent {kek_epoch, rotation_op, fence_frontier,
    // control_frontier, recipients_hash} recovers from the persisted
    // frame, and I3 already serves the NEW epoch.
    {
        let mut log = Log::new();
        let a = log.commit(1, 1);
        let (f, fr) = log.fence(1, 2);
        log.rewrap(1, a, 2);
        out.push(ec_vector(
            "erase-crash-state2-fence-intent-recovered",
            &log.stream,
            vec![fr.end],
            2,
            json!({ "state_probes": [
                probe("fence.recovered", &f.to_value()),
                probe("serving.epoch", &Value::Uint(2)),
            ] }),
        ));
    }

    // State 3: one of two survivor rewraps durable, the second torn —
    // recovery resumes the (idempotent, I2) rewrap pass.
    {
        let mut log = Log::new();
        let a = log.commit(1, 1);
        let b = log.commit(2, 1);
        let (f, _) = log.fence(1, 2);
        let (wa, _) = log.rewrap(1, a, 2);
        let (_, rb) = log.rewrap(2, b, 2);
        log.rewrap_done(&f, &[a, b]);
        out.push(ec_vector(
            "erase-crash-state3-rewrap-progress",
            &log.stream,
            vec![rb.start + 7],
            3,
            json!({ "state_probes": [
                probe("rewrap.recovered", &wa.to_value()),
                probe("serving.epoch", &Value::Uint(2)),
            ] }),
        ));
    }

    // State 4 = the D-73 third-rotation vector: item a committed at
    // epoch 1 survives rotations 1→2→3→4 and sits in the THIRD
    // rotation's survivor set wrapper-current (original commit epoch
    // irrelevant); item d commits post-Fence under the new epoch and
    // stays outside the set by construction (D-67).
    {
        let mut log = Log::new();
        let a = log.commit(1, 1);
        let (f2, _) = log.fence(1, 2);
        log.rewrap(1, a, 2);
        log.rewrap_done(&f2, &[a]);
        log.kek_destroyed(1);
        let b = log.commit(2, 2);
        let (f3, _) = log.fence(2, 3);
        log.rewrap(1, a, 3);
        log.rewrap(2, b, 3);
        log.rewrap_done(&f3, &[a, b]);
        log.kek_destroyed(2);
        let (f4, _) = log.fence(3, 4);
        log.commit(3, 4); // post-Fence commit: NEW epoch, not a survivor
        log.rewrap(1, a, 4);
        log.rewrap(2, b, 4);
        let (set, done) = log.rewrap_done(&f4, &[a, b]);
        out.push(ec_vector(
            "erase-crash-state4-third-rotation-epoch1-survivor",
            &log.stream,
            vec![done.end],
            4,
            json!({ "state_probes": [
                probe("survivorset.recomputed", &set.to_value()),
                probe("serving.epoch", &Value::Uint(4)),
            ] }),
        ));
    }

    // State 5: crash between KekDestroyed and the tombstone —
    // recovery re-derives the missing tombstone from the manifest
    // (retired_epoch = new_epoch − 1) and re-enters at 5.
    {
        let (log, kd_end, (t, _)) = erase_log();
        out.push(ec_vector(
            "erase-crash-state5-tombstone-rederivation",
            &log.stream,
            vec![kd_end],
            5,
            json!({ "state_probes": [
                probe("tombstones.rederived", &Value::Array(vec![t.to_value()])),
                probe("serving.epoch", &Value::Uint(2)),
            ] }),
        ));
    }

    // State 6: every tombstone durable — the rotation is complete.
    {
        let (log, _, (t, _)) = erase_log();
        let len = log.stream.len();
        out.push(ec_vector(
            "erase-crash-state6-complete",
            &log.stream,
            vec![len],
            6,
            json!({ "state_probes": [
                probe("tombstones.durable", &Value::Array(vec![t.to_value()])),
                probe("serving.epoch", &Value::Uint(2)),
            ] }),
        ));
    }

    // Survivor-completeness negative: c holds an epoch-1 wrap at the
    // Fence, is in no manifest, and the durable RewrapDone omits it —
    // "omission of any survivor blocks destruction" (§5.5): the
    // false completeness commitment is a log invariant violation and
    // the store quarantines. The conformant prefix ends at state 3.
    {
        let mut log = Log::new();
        let a = log.commit(1, 1);
        log.commit(3, 1); // c — never rewrapped, never tombstoned
        let (f, _) = log.fence(1, 2);
        log.rewrap(1, a, 2);
        log.rewrap_done(&f, &[a]);
        let len = log.stream.len();
        out.push(ec_vector(
            "erase-crash-survivor-omission-blocks-destruction",
            &log.stream,
            vec![len],
            3,
            json!({ "outcomes": [{
                "name": "survivor-omission-blocks-kek-destruction",
                "outcome": "log-corrupt",
                "disposition": "storage-quarantine",
            }] }),
        ));
    }

    // Rotation-queue serialization negative (D-89): rotation N's
    // tombstone is still pending when rotation N+1's Fence lands —
    // non-conformant; the conformant prefix records state 5.
    {
        let mut log = Log::new();
        let a = log.commit(1, 1);
        let b = log.commit(2, 1);
        let (f, _) = log.fence(1, 2);
        log.rewrap(1, a, 2);
        log.rewrap_done(&f, &[a]);
        log.kek_destroyed(1);
        log.fence(2, 3); // VIOLATION: b's tombstone is not durable yet
        log.tombstone(b, 1);
        let len = log.stream.len();
        out.push(ec_vector(
            "erase-crash-fence-before-tombstones-nonconformant",
            &log.stream,
            vec![len],
            5,
            json!({ "outcomes": [{
                "name": "n-plus-1-fence-before-state-6",
                "outcome": "log-corrupt",
                "disposition": "storage-quarantine",
            }] }),
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_erase_corpus_matches_builders() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("vectors");
        for v in corpus_erase() {
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
    fn erase_corpus_checks_clean() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let companion: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("d0a-vector-cases.v1.json")).unwrap(),
        )
        .unwrap();
        for v in corpus_erase() {
            crate::vector::check(&v.to_json(), &companion)
                .unwrap_or_else(|e| panic!("{}: {e}", v.name));
        }
    }

    /// Every cut lands inside its stream (frame-level validity is
    /// the reducer's side of the differential — core has no walker).
    #[test]
    fn erase_cuts_in_bounds() {
        for v in corpus_erase() {
            let stream_hex = v.inputs["stream"].as_str().unwrap();
            let len = stream_hex.len() / 2;
            for cut in v.inputs["cuts"].as_array().unwrap() {
                assert!(cut.as_u64().unwrap() as usize <= len, "{}", v.name);
            }
        }
    }
}
