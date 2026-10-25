//! Family 13's erase-crash-matrix lane — the §5.5 six-state
//! rotation/erase machine replayed over a tenant zone log with crash
//! cuts (the reducer's own replay; core only mints the frames).
//!
//! Lane conventions (fixture-defined, mirrored from the corpus
//! module): `stream` = ONE kind-0 tenant log; each `cut` truncates to
//! the durable prefix (L1); `machine_state` = the state recovery
//! re-enters at EVERY cut, derived from the CONFORMANT prefix
//! (frames before the first violation). `rotation_ops` = the SIGNED
//! `c.kek_rotate` operations the control plane accepted — the §5.5
//! control context: every durable Fence must resolve its rotation
//! through `Fence.rotation_op = H_op` over one supplied triple (the
//! hash covers the signature bytes; verifying the signature itself
//! needs the control fold's key material, which is fold territory).
//! Everything erase-shaped binds to the RESOLVED op's typed
//! `erase_manifest`: durable tombstones must match their manifest
//! entry exactly, state 5 vs 6 is manifest completeness, and state-5
//! recovery CONSTRUCTS the missing tombstones from the manifest
//! (retired_epoch = new_epoch − 1) — nothing is read back from
//! unsigned stream content. Fence commitment fields other than
//! `rotation_op` stay opaque here — mirror-checked against
//! RewrapDone (D-97/D-106) and probe-recovered; their derivation is
//! fold territory. A violation quarantines the store read-only
//! (§6.2): replay stops at the first one.

use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

use crate::cbor::{self, Node};
use crate::edge::{walk, HEADER_LEN};
use crate::envelope;
use crate::harness::SemStatus;
use crate::kat::{encode, Enc};

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd hex".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

fn keys_are(n: &Node, want: &[&str]) -> bool {
    n.map_keys().is_some_and(|mut k| {
        k.sort_unstable();
        let mut w = want.to_vec();
        w.sort_unstable();
        k == w
    })
}

// ------------------------------------------------- frame extraction

struct Frame<'a> {
    ftype: u8,
    payload: &'a [u8],
}

/// Walk a stream into typed frames; the tail may be torn (truncated
/// away). `None` = framing corruption (not an erase-lane concern —
/// the fixtures are frame-valid).
fn frames_of(stream: &[u8]) -> Option<Vec<Frame<'_>>> {
    let (spans, _durable) = walk(stream)?;
    Some(
        spans
            .iter()
            .map(|&(start, end)| Frame {
                // SYNC(4) ‖ len(4) ‖ ~len(4) ‖ type(1) ‖ payload ‖ crc(4)
                ftype: stream[start + 12],
                payload: &stream[start + 13..end - 4],
            })
            .collect(),
    )
}

// ------------------------------------- the signed rotation context

/// One accepted `c.kek_rotate`, resolved by `Fence.rotation_op =
/// H_op` — the §5.5 control-side context this lane binds erasure to.
struct RotOp {
    plane_id: [u8; 32],
    zone_id: [u8; 16],
    new_epoch: u64,
    /// `item_addr → (erase_op, target_op)` — the typed manifest.
    manifest: BTreeMap<[u8; 32], ([u8; 32], [u8; 32])>,
}

/// Parse `inputs.rotation_ops` into `H_op → RotOp`. Structural
/// validation only (canonical triple, body_hash binding, exact body
/// key set, typed sorted manifest): the fixture contract supplies
/// ACCEPTED ops, so malformation here is a fixture error, not a
/// machine outcome.
fn rotation_index(hexes: &[Json]) -> Result<BTreeMap<[u8; 32], RotOp>, String> {
    let mut index = BTreeMap::new();
    for (i, h) in hexes.iter().enumerate() {
        let raw = unhex(h.as_str().ok_or("rotation_ops entry not hex")?)?;
        let op = envelope::parse_op(&raw).map_err(|e| format!("rotation_ops[{i}]: {e:?}"))?;
        if !op.body_hash_ok() {
            return Err(format!("rotation_ops[{i}]: body_hash mismatch"));
        }
        if op.header.operation_type != "c.kek_rotate" {
            return Err(format!("rotation_ops[{i}]: not a c.kek_rotate"));
        }
        let body = &op.body;
        if !keys_are(body, &["zone_id", "new_epoch", "wraps", "erase_manifest"]) {
            return Err(format!("rotation_ops[{i}]: body key set"));
        }
        let zone_id = body
            .get("zone_id")
            .and_then(|v| v.bytes_n::<16>())
            .ok_or(format!("rotation_ops[{i}]: zone_id"))?;
        let new_epoch = body
            .get("new_epoch")
            .and_then(|v| v.as_uint())
            .ok_or(format!("rotation_ops[{i}]: new_epoch"))?;
        match body.get("wraps").and_then(|w| w.as_array()) {
            Some(a) if !a.is_empty() => {} // [+ kekwrap]; internals are fold territory
            _ => return Err(format!("rotation_ops[{i}]: empty wraps")),
        }
        let entries = body
            .get("erase_manifest")
            .and_then(|m| m.as_array())
            .ok_or(format!("rotation_ops[{i}]: erase_manifest"))?;
        let mut manifest: BTreeMap<[u8; 32], ([u8; 32], [u8; 32])> = BTreeMap::new();
        let mut prev_addr: Option<[u8; 32]> = None;
        for e in entries {
            if !keys_are(e, &["item_addr", "erase_op", "target_op"]) {
                return Err(format!("rotation_ops[{i}]: erasemref key set"));
            }
            let b32 = |k: &str| -> Result<[u8; 32], String> {
                e.get(k)
                    .and_then(|v| v.bytes_n::<32>())
                    .ok_or(format!("rotation_ops[{i}]: erasemref.{k}"))
            };
            let addr = b32("item_addr")?;
            // E7 set, keyed and sorted by item_addr.
            if prev_addr.is_some_and(|p| p >= addr) {
                return Err(format!(
                    "rotation_ops[{i}]: erase_manifest not a sorted set"
                ));
            }
            prev_addr = Some(addr);
            manifest.insert(addr, (b32("erase_op")?, b32("target_op")?));
        }
        if index
            .insert(
                op.op_hash(),
                RotOp {
                    plane_id: op.header.plane_id,
                    zone_id,
                    new_epoch,
                    manifest,
                },
            )
            .is_some()
        {
            return Err(format!("rotation_ops[{i}]: duplicate op"));
        }
    }
    Ok(index)
}

// -------------------------------------------------- payload parsing

struct WrapF<'a> {
    item_addr: [u8; 32],
    epoch: u64,
    raw: &'a [u8],
}

fn parse_wrap<'a>(n: &Node<'a>) -> Result<WrapF<'a>, String> {
    if !keys_are(n, &["v", "item_addr", "key_wrap_epoch", "wrapped_dek"]) {
        return Err("itemwrap key set".into());
    }
    if n.get("v").and_then(|v| v.as_uint()) != Some(1) {
        return Err("itemwrap v".into());
    }
    Ok(WrapF {
        item_addr: n
            .get("item_addr")
            .and_then(|v| v.bytes_n::<32>())
            .ok_or("itemwrap.item_addr")?,
        epoch: n
            .get("key_wrap_epoch")
            .and_then(|v| v.as_uint())
            .ok_or("itemwrap.key_wrap_epoch")?,
        raw: n.raw,
    })
}

/// The five Fence commitment fields (RewrapDone mirrors them, D-97).
#[derive(PartialEq, Eq)]
struct FenceFields {
    kek_epoch: u64,
    rotation_op: [u8; 32],
    fence_frontier: [u8; 32],
    control_frontier: [u8; 32],
    recipients_hash: [u8; 32],
}

fn fence_fields(n: &Node) -> Result<FenceFields, String> {
    let b32 = |k: &str| -> Result<[u8; 32], String> {
        n.get(k)
            .and_then(|v| v.bytes_n::<32>())
            .ok_or(format!("fence field {k}"))
    };
    Ok(FenceFields {
        kek_epoch: n
            .get("kek_epoch")
            .and_then(|v| v.as_uint())
            .ok_or("kek_epoch")?,
        rotation_op: b32("rotation_op")?,
        fence_frontier: b32("fence_frontier")?,
        control_frontier: b32("control_frontier")?,
        recipients_hash: b32("recipients_hash")?,
    })
}

// ---------------------------------------------------------- replay

/// One in-flight or completed rotation as the log records it,
/// carrying its RESOLVED signed context.
struct Rotation<'a> {
    new_epoch: u64,
    fence_raw: &'a [u8],
    fence: FenceFields,
    /// The resolved op's typed manifest — the erase authority.
    manifest: BTreeMap<[u8; 32], ([u8; 32], [u8; 32])>,
    expected: BTreeSet<[u8; 32]>,
    rewrap_count: usize,
    /// The recomputed canonical survivorset once RewrapDone verifies.
    recomputed: Option<Vec<u8>>,
    done: bool,
    kd: bool,
    tombs: BTreeMap<[u8; 32], &'a [u8]>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Violation {
    outcome: &'static str,
    disposition: &'static str,
    detail: String,
}

fn quarantine(outcome: &'static str, detail: impl Into<String>) -> Violation {
    Violation {
        outcome,
        disposition: "storage-quarantine",
        detail: detail.into(),
    }
}

/// Everything recovery derives from one durable prefix.
struct Replayed<'a> {
    rotations: Vec<Rotation<'a>>,
    last_rewrap_raw: Option<&'a [u8]>,
    violation: Option<Violation>,
}

fn replay<'a>(
    frames: &[Frame<'a>],
    index: &BTreeMap<[u8; 32], RotOp>,
    log_plane: [u8; 32],
    log_zone: [u8; 16],
) -> Result<Replayed<'a>, String> {
    let mut wraps: BTreeMap<[u8; 32], u64> = BTreeMap::new();
    let mut seen_wraps: BTreeMap<([u8; 32], u64), &'a [u8]> = BTreeMap::new();
    let mut tombstoned: BTreeSet<[u8; 32]> = BTreeSet::new();
    let mut rotations: Vec<Rotation<'a>> = Vec::new();
    let mut last_rewrap_raw: Option<&'a [u8]> = None;
    let mut violation: Option<Violation> = None;

    // Record a wrap; a differing duplicate for one (addr, epoch) is
    // corruption or fork evidence (I2).
    let record_wrap = |wraps: &mut BTreeMap<[u8; 32], u64>,
                       seen: &mut BTreeMap<([u8; 32], u64), &'a [u8]>,
                       w: &WrapF<'a>|
     -> Option<Violation> {
        if let Some(prev) = seen.get(&(w.item_addr, w.epoch)) {
            if *prev != w.raw {
                return Some(Violation {
                    outcome: "wrapper-mismatch",
                    disposition: "storage-quarantine",
                    detail: "differing duplicate wrapper for one (item_addr, epoch)".into(),
                });
            }
        }
        seen.insert((w.item_addr, w.epoch), w.raw);
        wraps.insert(w.item_addr, w.epoch);
        None
    };

    'frames: for f in frames {
        let n = cbor::decode(f.payload).map_err(|e| format!("frame payload: {e:?}"))?;
        match f.ftype {
            0x11 => {
                if !keys_are(&n, &["core", "wrap", "lineage", "gen", "seq"]) {
                    return Err("itemcommit key set".into());
                }
                let w = parse_wrap(n.get("wrap").ok_or("itemcommit.wrap")?)?;
                if let Some(v) = record_wrap(&mut wraps, &mut seen_wraps, &w) {
                    violation = Some(v);
                    break 'frames;
                }
            }
            0x12 => {
                if !keys_are(&n, &["wrap"]) {
                    return Err("itemrewrap key set".into());
                }
                let w = parse_wrap(n.get("wrap").ok_or("itemrewrap.wrap")?)?;
                let raw = w.raw;
                if let Some(v) = record_wrap(&mut wraps, &mut seen_wraps, &w) {
                    violation = Some(v);
                    break 'frames;
                }
                last_rewrap_raw = Some(raw);
                if let Some(r) = rotations.last_mut() {
                    if !r.done {
                        r.rewrap_count += 1;
                    }
                }
            }
            0x13 => {
                let fence = fence_fields(&n)?;
                // The de-oracled binding: the Fence must resolve its
                // ACCEPTED rotation op — a store never Fences an op
                // it does not hold (state 1 precedes state 2), so an
                // unresolvable rotation_op is a fixture-contract
                // error, not a machine outcome.
                let Some(rot_op) = index.get(&fence.rotation_op) else {
                    return Err("Fence.rotation_op resolves no supplied rotation op".into());
                };
                // The resolved op must govern THIS store's zone and
                // agree on the activated epoch — a Fence citing
                // another zone's rotation or a different epoch is
                // local log corruption.
                if rot_op.plane_id != log_plane || rot_op.zone_id != log_zone {
                    violation = Some(quarantine(
                        "log-corrupt",
                        "Fence resolves a rotation for another plane/zone",
                    ));
                    break 'frames;
                }
                if fence.kek_epoch != rot_op.new_epoch {
                    violation = Some(quarantine(
                        "log-corrupt",
                        "Fence epoch differs from its rotation op",
                    ));
                    break 'frames;
                }
                // Serialization (D-89): the predecessor rotation must
                // have completed state 6 (KEK destroyed + every
                // manifest tombstone durable).
                if let Some(prev) = rotations.last() {
                    let complete = prev.done
                        && prev.kd
                        && prev.manifest.keys().all(|a| prev.tombs.contains_key(a));
                    if !complete {
                        violation = Some(violation_fence_before_six());
                        break 'frames;
                    }
                    if fence.kek_epoch != prev.new_epoch + 1 {
                        violation =
                            Some(quarantine("log-corrupt", "non-consecutive rotation epoch"));
                        break 'frames;
                    }
                } else if fence.kek_epoch != 2 {
                    // Epoch 1 is active from creation with no Fence
                    // (D-92): the first rotation activates epoch 2.
                    violation = Some(quarantine(
                        "log-corrupt",
                        "first rotation must fence epoch 2",
                    ));
                    break 'frames;
                }
                // Freeze the expected membership (D-73,
                // wrapper-current): non-tombstoned holders of the
                // retiring epoch at the Fence, minus the SIGNED
                // manifest's item_addrs.
                let retiring = fence.kek_epoch - 1;
                let expected: BTreeSet<[u8; 32]> = wraps
                    .iter()
                    .filter(|(addr, epoch)| {
                        **epoch == retiring
                            && !tombstoned.contains(*addr)
                            && !rot_op.manifest.contains_key(*addr)
                    })
                    .map(|(addr, _)| *addr)
                    .collect();
                rotations.push(Rotation {
                    new_epoch: fence.kek_epoch,
                    fence_raw: f.payload,
                    fence,
                    manifest: rot_op.manifest.clone(),
                    expected,
                    rewrap_count: 0,
                    recomputed: None,
                    done: false,
                    kd: false,
                    tombs: BTreeMap::new(),
                });
            }
            0x14 => {
                let done = fence_fields(&n)?;
                let count = n.get("count").and_then(|v| v.as_uint()).ok_or("count")?;
                let survivors = n
                    .get("survivors")
                    .and_then(|v| v.bytes_n::<32>())
                    .ok_or("survivors")?;
                let Some(r) = rotations.last_mut() else {
                    violation = Some(quarantine("log-corrupt", "RewrapDone without a Fence"));
                    break 'frames;
                };
                if r.done {
                    violation = Some(quarantine("log-corrupt", "duplicate RewrapDone"));
                    break 'frames;
                }
                if done != r.fence {
                    // The RewrapDone mirrors the Fence exactly
                    // (D-97/D-106).
                    violation = Some(quarantine(
                        "log-corrupt",
                        "RewrapDone does not mirror Fence",
                    ));
                    break 'frames;
                }
                // Recompute the survivorset from the durable wrappers
                // (§5.5 states 4–5: completeness is provable).
                let mut pairs: Vec<Enc> = Vec::new();
                for addr in &r.expected {
                    let Some(wrap_raw) = seen_wraps.get(&(*addr, r.new_epoch)) else {
                        violation = Some(quarantine(
                            "log-corrupt",
                            "survivor omission blocks KEK destruction",
                        ));
                        break 'frames;
                    };
                    let wrap_hash: [u8; 32] = Sha256::digest(wrap_raw).into();
                    pairs.push(Enc::M(vec![
                        ("item_addr", Enc::B(addr.to_vec())),
                        ("wrap_hash", Enc::B(wrap_hash.to_vec())),
                    ]));
                }
                let set = encode(&Enc::M(vec![("v", Enc::U(1)), ("pairs", Enc::A(pairs))]));
                if count != r.expected.len() as u64
                    || crate::domains::h("survivors", &set) != survivors
                {
                    violation = Some(quarantine(
                        "log-corrupt",
                        "RewrapDone commitment differs from the recomputed survivor set",
                    ));
                    break 'frames;
                }
                r.recomputed = Some(set);
                r.done = true;
            }
            0x1a => {
                if !keys_are(&n, &["epoch"]) {
                    return Err("kekdestroyed key set".into());
                }
                let epoch = n.get("epoch").and_then(|v| v.as_uint()).ok_or("epoch")?;
                let Some(r) = rotations.last_mut() else {
                    violation = Some(quarantine("log-corrupt", "KekDestroyed without a rotation"));
                    break 'frames;
                };
                if !r.done {
                    // Destruction before a verified RewrapComplete is
                    // non-conformant (§5.5).
                    violation = Some(quarantine(
                        "log-corrupt",
                        "KEK destruction before a verified RewrapComplete",
                    ));
                    break 'frames;
                }
                if r.kd || epoch != r.new_epoch - 1 {
                    // KekDestroyed.epoch = the destroyed epoch =
                    // new − 1 (D-92).
                    violation = Some(quarantine("log-corrupt", "KekDestroyed epoch (D-92)"));
                    break 'frames;
                }
                r.kd = true;
            }
            0x15 => {
                if !keys_are(
                    &n,
                    &["v", "item_addr", "erase_op", "target_op", "retired_epoch"],
                ) {
                    return Err("tombstone key set".into());
                }
                let b32 = |k: &'static str| -> Result<[u8; 32], String> {
                    n.get(k)
                        .and_then(|v| v.bytes_n::<32>())
                        .ok_or(format!("tombstone.{k}"))
                };
                let addr = b32("item_addr")?;
                let erase_op = b32("erase_op")?;
                let target_op = b32("target_op")?;
                let retired = n
                    .get("retired_epoch")
                    .and_then(|v| v.as_uint())
                    .ok_or("tombstone.retired_epoch")?;
                let Some(r) = rotations.last_mut() else {
                    violation = Some(quarantine("log-corrupt", "Tombstone without a rotation"));
                    break 'frames;
                };
                if !r.kd || retired != r.new_epoch - 1 {
                    // Tombstones follow destruction with
                    // retired_epoch = new − 1 (§5.5 state 6).
                    violation = Some(quarantine("log-corrupt", "tombstone out of machine order"));
                    break 'frames;
                }
                // The erase authority is the SIGNED manifest: a
                // tombstone for an unmanifested item, or one whose
                // typed fields differ from its entry, is corruption.
                let Some(&(want_erase, want_target)) = r.manifest.get(&addr) else {
                    violation = Some(quarantine(
                        "log-corrupt",
                        "tombstone for an item outside the rotation's erase_manifest",
                    ));
                    break 'frames;
                };
                if (erase_op, target_op) != (want_erase, want_target) {
                    violation = Some(quarantine(
                        "log-corrupt",
                        "tombstone differs from its signed manifest entry",
                    ));
                    break 'frames;
                }
                tombstoned.insert(addr);
                r.tombs.insert(addr, f.payload);
            }
            0x01 => {
                // Tenant logs have NO plaintext-operation record type
                // (§6.1).
                violation = Some(quarantine("log-corrupt", "control record in a tenant log"));
                break 'frames;
            }
            0x16 | 0x17 => {} // receipts/outbox marks are inert here
            other => {
                return Err(format!("frame type {other:#x} in the erase-crash lane"));
            }
        }
    }

    Ok(Replayed {
        rotations,
        last_rewrap_raw,
        violation,
    })
}

fn violation_fence_before_six() -> Violation {
    quarantine(
        "log-corrupt",
        "N+1 Fence before rotation N completed state 6 (D-89)",
    )
}

/// The §5.5 recorded state of the conformant prefix. State 5 vs 6 is
/// the SIGNED manifest's completeness against durable tombstones.
fn machine_state(r: &Replayed) -> u64 {
    let Some(rot) = r.rotations.last() else {
        return 1; // acceptance is control-side; no durable activation
    };
    if !rot.done {
        return if rot.rewrap_count == 0 { 2 } else { 3 };
    }
    if !rot.kd {
        return 4;
    }
    if rot.manifest.keys().all(|a| rot.tombs.contains_key(a)) {
        6
    } else {
        5
    }
}

/// One CBOR array over already-canonical element encodings.
fn cbor_array(raws: &[&[u8]]) -> Vec<u8> {
    assert!(raws.len() < 24, "erase-lane probe arrays stay short");
    let mut out = vec![0x80 | raws.len() as u8];
    for r in raws {
        out.extend_from_slice(r);
    }
    out
}

#[derive(PartialEq, Eq, Debug)]
struct CutResult {
    state: u64,
    violations: Vec<(String, String)>,
    probes: BTreeMap<String, Vec<u8>>,
}

fn run_cut(
    prefix: &[u8],
    index: &BTreeMap<[u8; 32], RotOp>,
    log_plane: [u8; 32],
    log_zone: [u8; 16],
) -> Result<CutResult, String> {
    let frames = frames_of(prefix).ok_or("durable prefix fails the walk")?;
    let replayed = replay(&frames, index, log_plane, log_zone)?;
    let state = machine_state(&replayed);

    let mut probes: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let serving = replayed.rotations.last().map(|r| r.new_epoch).unwrap_or(1); // I3: the last-Fenced epoch; epoch 1 sans Fence
    probes.insert("serving.epoch".into(), encode(&Enc::U(serving)));
    if let Some(rot) = replayed.rotations.last() {
        probes.insert("fence.recovered".into(), rot.fence_raw.to_vec());
        if let Some(set) = &rot.recomputed {
            probes.insert("survivorset.recomputed".into(), set.clone());
        }
        if state == 5 {
            // Recovery at 5 re-derives the missing tombstones from
            // the SIGNED manifest (§5.5 state 6): each entry yields
            // { v, item_addr, erase_op, target_op, retired_epoch =
            // new − 1 } — constructed, never read from the stream.
            let missing: Vec<Vec<u8>> = rot
                .manifest
                .iter()
                .filter(|(addr, _)| !rot.tombs.contains_key(*addr))
                .map(|(addr, ent)| {
                    encode(&Enc::M(vec![
                        ("v", Enc::U(1)),
                        ("item_addr", Enc::B(addr.to_vec())),
                        ("erase_op", Enc::B(ent.0.to_vec())),
                        ("target_op", Enc::B(ent.1.to_vec())),
                        ("retired_epoch", Enc::U(rot.new_epoch - 1)),
                    ]))
                })
                .collect();
            let refs: Vec<&[u8]> = missing.iter().map(|v| v.as_slice()).collect();
            probes.insert("tombstones.rederived".into(), cbor_array(&refs));
        }
        if !rot.tombs.is_empty() {
            let durable: Vec<&[u8]> = rot.tombs.values().copied().collect();
            probes.insert("tombstones.durable".into(), cbor_array(&durable));
        }
    }
    if let Some(raw) = replayed.last_rewrap_raw {
        probes.insert("rewrap.recovered".into(), raw.to_vec());
    }

    Ok(CutResult {
        state,
        violations: replayed
            .violation
            .into_iter()
            .map(|v| (v.outcome.to_string(), v.disposition.to_string()))
            .collect(),
        probes,
    })
}

// ------------------------------------------------------- lane entry

pub fn erase_crash_matrix(vector: &Json) -> Result<SemStatus, String> {
    let stream = unhex(vector["inputs"]["stream"].as_str().ok_or("inputs.stream")?)?;
    let cuts: Vec<usize> = vector["inputs"]["cuts"]
        .as_array()
        .ok_or("inputs.cuts")?
        .iter()
        .map(|c| c.as_u64().map(|n| n as usize).ok_or("cut"))
        .collect::<Result<_, _>>()?;
    let want_state = vector["inputs"]["machine_state"]
        .as_u64()
        .ok_or("inputs.machine_state")?;
    let index = rotation_index(
        vector["inputs"]["rotation_ops"]
            .as_array()
            .ok_or("inputs.rotation_ops")?,
    )?;

    if stream.len() < HEADER_LEN {
        return Err("stream shorter than the file header".into());
    }
    // `IPLOG2` ‖ version ‖ kind ‖ plane_id(32) ‖ zone_id(16) — the
    // store identity the resolved rotations must govern.
    let log_plane: [u8; 32] = stream[8..40].try_into().expect("32-byte slice");
    let log_zone: [u8; 16] = stream[40..56].try_into().expect("16-byte slice");
    frames_of(&stream).ok_or("full stream fails the walk")?;

    // Every cut must recover identically.
    let mut result: Option<CutResult> = None;
    for cut in &cuts {
        if *cut > stream.len() {
            return Err("cut beyond the stream".into());
        }
        let out = run_cut(&stream[..*cut], &index, log_plane, log_zone)?;
        match &result {
            None => result = Some(out),
            Some(first) if *first == out => {}
            Some(_) => {
                return Ok(SemStatus::Fail(
                    "cuts disagree on the recovered state".into(),
                ))
            }
        }
    }
    let result = result.ok_or("no cuts")?;

    if result.state != want_state {
        return Ok(SemStatus::Fail(format!(
            "machine_state: expected {want_state}, reducer derived {}",
            result.state
        )));
    }

    // outcomes rows: the violations in stream order (absent = none).
    let want_rows: Vec<(String, String)> = match vector["expected"]["result"]["outcomes"].as_array()
    {
        None => Vec::new(),
        Some(rows) => rows
            .iter()
            .map(|r| {
                Ok((
                    r["outcome"].as_str().ok_or("row.outcome")?.to_string(),
                    r["disposition"]
                        .as_str()
                        .ok_or("row.disposition")?
                        .to_string(),
                ))
            })
            .collect::<Result<_, &str>>()?,
    };
    if want_rows != result.violations {
        return Ok(SemStatus::Fail(format!(
            "violations: expected {want_rows:?}, reducer derived {:?}",
            result.violations
        )));
    }

    // state_probes: exact-name registry, canonical-byte equality.
    if let Some(rows) = vector["expected"]["result"]["state_probes"].as_array() {
        for p in rows {
            let name = p["name"].as_str().ok_or("probe.name")?;
            let want = p["value"].as_str().ok_or("probe.value")?;
            let Some(got) = result.probes.get(name) else {
                return Ok(SemStatus::Unimplemented(format!("state probe {name:?}")));
            };
            let got_hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
            if got_hex != want {
                return Ok(SemStatus::Fail(format!(
                    "probe {name:?}: expected {want}, got {got_hex}"
                )));
            }
        }
    }

    Ok(SemStatus::Pass)
}
