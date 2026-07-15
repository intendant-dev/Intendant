//! Family 12 (§10.1 service-edge admission) and family 13's framing
//! lanes (§6.2 file framing v2) — the reducer's own edge predicate,
//! CRC32C, and frame walker.
//!
//! Edge fixture shape (the corpus convention; the companion leaves
//! `context`/`request` free-form): `context` carries enrolled and
//! revoked devices, sessions, tokens, per-op quotas; `request`
//! carries the §10.1 shape number plus its components. Check order —
//! identity → session → token (present → revoked → scope) → quota;
//! a component that cannot exist for a shape is not in its rule.

use serde_json::Value as Json;

use crate::harness::SemStatus;

// ------------------------------------------------------- family 12

fn ctx_list<'a>(context: &'a Json, key: &str) -> Vec<&'a Json> {
    context[key]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// The §10.1 edge predicate. `Ok(())` = allowed; `Err` = the single
/// (outcome, disposition) per multi-fault precedence.
fn admit_edge(
    context: &Json,
    request: &Json,
) -> Result<Result<(), (&'static str, &'static str)>, String> {
    let deny = |o: &'static str| Ok(Err((o, "edge-deny")));
    let shape = request["shape"].as_u64().ok_or("request.shape")?;
    let op = request["op"].as_str().ok_or("request.op")?;

    // Identity leg (shapes 1 and 2 name a device).
    if matches!(shape, 1 | 2) {
        let device = request["device"].as_str().ok_or("request.device")?;
        if ctx_list(context, "revoked")
            .iter()
            .any(|d| d.as_str() == Some(device))
        {
            return deny("cert-revoked");
        }
        if !ctx_list(context, "devices")
            .iter()
            .any(|d| d.as_str() == Some(device))
        {
            return deny("no-cert");
        }
    }

    // Session leg (shapes 2 and 4 name a session; shape 3's rides
    // its token).
    let session_live = |id: &str| -> Option<bool> {
        ctx_list(context, "sessions")
            .iter()
            .find(|s| s["id"].as_str() == Some(id))
            .map(|s| s["live"].as_bool() == Some(true))
    };
    if matches!(shape, 2 | 4) {
        let sid = request["session"].as_str().ok_or("request.session")?;
        match session_live(sid) {
            None => return deny("no-session"),
            Some(false) => return deny("session-ended"),
            Some(true) => {}
        }
    }

    // Token leg (shapes 3 and 4).
    if matches!(shape, 3 | 4) {
        let tid = request["token"].as_str().ok_or("request.token")?;
        let Some(token) = ctx_list(context, "tokens")
            .into_iter()
            .find(|t| t["token"].as_str() == Some(tid))
        else {
            return deny("no-token");
        };
        if token["revoked"].as_bool() == Some(true) {
            return deny("token-revoked");
        }
        // The token's session must be live (shape 3's session comes
        // from the token itself).
        if let Some(sid) = token["session"].as_str() {
            match session_live(sid) {
                None => return deny("no-session"),
                Some(false) => return deny("session-ended"),
                Some(true) => {}
            }
        }
        let in_scope = token["scope"]
            .as_array()
            .is_some_and(|s| s.iter().any(|v| v.as_str() == Some(op)));
        if !in_scope {
            return deny("token-scope");
        }
    }

    if !matches!(shape, 1..=4) {
        return Err(format!("unknown edge shape {shape}"));
    }

    // Quota leg — per-SHAPE service policy (§10.1): a named op with
    // zero remaining denies for that shape only.
    if let Some(q) = context["quota"][shape.to_string()].get(op) {
        if q.as_u64() == Some(0) {
            return deny("quota");
        }
    }
    Ok(Ok(()))
}

pub fn edge_admission(vector: &Json) -> Result<SemStatus, String> {
    let context = &vector["inputs"]["context"];
    let request = &vector["inputs"]["request"];
    match admit_edge(context, request)? {
        Ok(()) => {
            if vector["expected"]["result"]["allowed"].as_bool() == Some(true) {
                Ok(SemStatus::Pass)
            } else {
                Ok(SemStatus::Fail("reducer allowed a deny vector".into()))
            }
        }
        Err((o, d)) => {
            let (Some(wo), Some(wd)) = (
                vector["expected"]["outcome"].as_str(),
                vector["expected"]["disposition"].as_str(),
            ) else {
                return Ok(SemStatus::Fail(format!(
                    "reducer denied ({o}, {d}) but the vector expects allow"
                )));
            };
            if (o, d) == (wo, wd) {
                Ok(SemStatus::Pass)
            } else {
                Ok(SemStatus::Fail(format!(
                    "expected ({wo}, {wd}), reducer derived ({o}, {d})"
                )))
            }
        }
    }
}

// ------------------------------------------------------- family 13

/// CRC32C (Castagnoli, RFC 3720 convention) — the reducer's own.
fn crc32c(data: &[u8]) -> u32 {
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
pub const HEADER_LEN: usize = 6 + 1 + 1 + 32 + 16;

/// One walk step: a complete valid frame, a torn tail (EOF inside a
/// frame — everything present is prefix-consistent), or corruption.
enum Step {
    Frame { end: usize },
    Torn,
    Corrupt,
}

fn step(stream: &[u8], at: usize) -> Step {
    let rest = &stream[at..];
    if rest.len() < 4 {
        // A torn write leaves a prefix of the next frame's SYNC.
        return if SYNC.starts_with(rest) {
            Step::Torn
        } else {
            Step::Corrupt
        };
    }
    if &rest[..4] != SYNC {
        return Step::Corrupt;
    }
    if rest.len() < 12 {
        return Step::Torn;
    }
    let len = u32::from_le_bytes(rest[4..8].try_into().expect("4 bytes"));
    let nlen = u32::from_le_bytes(rest[8..12].try_into().expect("4 bytes"));
    if len != !nlen {
        // The redundant length is checked BEFORE seeking (§6.2).
        return Step::Corrupt;
    }
    let total = 12 + len as usize + 4;
    if rest.len() < total {
        return Step::Torn;
    }
    let body = &rest[4..12 + len as usize];
    let crc = u32::from_le_bytes(rest[12 + len as usize..total].try_into().expect("4 bytes"));
    if crc32c(body) != crc {
        // A complete frame with a bad CRC is ambiguous (torn vs
        // corrupted committed data): quarantine, final or not.
        return Step::Corrupt;
    }
    Step::Frame { end: at + total }
}

/// Validate the header, walk every frame. Returns the frame slices
/// and the durable prefix length, or `None` = corruption. Public:
/// the browser lane maps IndexedDB transaction boundaries onto the
/// REAL frame boundaries this walker reports.
pub fn walk(stream: &[u8]) -> Option<(Vec<(usize, usize)>, usize)> {
    if stream.len() < HEADER_LEN || &stream[..6] != b"IPLOG2" || stream[6] != 2 || stream[7] > 1 {
        return None;
    }
    let mut at = HEADER_LEN;
    let mut frames = Vec::new();
    loop {
        if at == stream.len() {
            return Some((frames, at));
        }
        match step(stream, at) {
            Step::Frame { end } => {
                frames.push((at, end));
                at = end;
            }
            // Torn tail: the durable prefix ends at the last
            // complete frame; the truncation is the recovery.
            Step::Torn => return Some((frames, at)),
            Step::Corrupt => return None,
        }
    }
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd hex".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

/// frame-roundtrip: the stream walks cleanly into EXACTLY the listed
/// frames with nothing torn, and re-serialization (header ‖ frames)
/// is byte-identical to the expected stream.
pub fn frame_roundtrip(vector: &Json) -> Result<SemStatus, String> {
    let stream = unhex(vector["inputs"]["stream"].as_str().ok_or("inputs.stream")?)?;
    let want_frames: Vec<Vec<u8>> = vector["inputs"]["frames"]
        .as_array()
        .ok_or("inputs.frames")?
        .iter()
        .map(|f| unhex(f.as_str().ok_or("frame not a string").unwrap_or_default()))
        .collect::<Result<_, _>>()?;
    let Some((frames, end)) = walk(&stream) else {
        return Ok(SemStatus::Fail("roundtrip stream fails the walk".into()));
    };
    if end != stream.len() {
        return Ok(SemStatus::Fail("roundtrip stream has a torn tail".into()));
    }
    let got: Vec<&[u8]> = frames.iter().map(|&(a, b)| &stream[a..b]).collect();
    if got.len() != want_frames.len()
        || got
            .iter()
            .zip(&want_frames)
            .any(|(g, w)| *g != w.as_slice())
    {
        return Ok(SemStatus::Fail("extracted frames differ".into()));
    }
    // Re-serialize: header ‖ frames == the expected bytes.
    let mut rebuilt = stream[..HEADER_LEN].to_vec();
    for f in &want_frames {
        rebuilt.extend_from_slice(f);
    }
    let want = unhex(
        vector["expected"]["bytes"]
            .as_str()
            .ok_or("expected.bytes")?,
    )?;
    if rebuilt == want && rebuilt == stream {
        Ok(SemStatus::Pass)
    } else {
        Ok(SemStatus::Fail("re-serialized stream differs".into()))
    }
}

fn apply_mutations(stream: &mut [u8], muts: &Json) -> Result<(), String> {
    for m in muts.as_array().ok_or("mutations")? {
        let off = m["offset"].as_u64().ok_or("mutation.offset")? as usize;
        let bytes = unhex(m["bytes"].as_str().ok_or("mutation.bytes")?)?;
        if off + bytes.len() > stream.len() {
            return Err("mutation out of range".into());
        }
        stream[off..off + bytes.len()].copy_from_slice(&bytes);
    }
    Ok(())
}

/// corruption-negative: the mutated stream must fail the walk.
pub fn corruption_negative(vector: &Json) -> Result<SemStatus, String> {
    let mut stream = unhex(vector["inputs"]["stream"].as_str().ok_or("inputs.stream")?)?;
    apply_mutations(&mut stream, &vector["inputs"]["mutations"])?;
    if walk(&stream).is_some() {
        return Ok(SemStatus::Fail("corrupted stream walked cleanly".into()));
    }
    let (wo, wd) = (
        vector["expected"]["outcome"].as_str().ok_or("outcome")?,
        vector["expected"]["disposition"]
            .as_str()
            .ok_or("disposition")?,
    );
    if (wo, wd) == ("log-corrupt", "storage-quarantine") {
        Ok(SemStatus::Pass)
    } else {
        Ok(SemStatus::Fail(format!(
            "corruption maps to (log-corrupt, storage-quarantine), vector expects ({wo}, {wd})"
        )))
    }
}

/// crash-replay: EOF at the cut truncates to the durable prefix
/// (torn writes are recovery, never corruption). One cut per vector.
pub fn crash_replay(vector: &Json) -> Result<SemStatus, String> {
    let stream = unhex(vector["inputs"]["stream"].as_str().ok_or("inputs.stream")?)?;
    let cuts = vector["inputs"]["cuts"].as_array().ok_or("inputs.cuts")?;
    let [cut] = cuts.as_slice() else {
        return Ok(SemStatus::Unimplemented(
            "multi-cut crash-replay vectors".into(),
        ));
    };
    let cut = cut.as_u64().ok_or("cut")? as usize;
    if cut > stream.len() {
        return Err("cut beyond the stream".into());
    }
    let Some((_frames, end)) = walk(&stream[..cut]) else {
        return Ok(SemStatus::Fail("cut stream reads as corruption".into()));
    };
    let want = vector["expected"]["result"]["truncated_at"]
        .as_u64()
        .ok_or("result.truncated_at")? as usize;
    if end == want {
        Ok(SemStatus::Pass)
    } else {
        Ok(SemStatus::Fail(format!(
            "durable prefix ends at {end}, expected {want}"
        )))
    }
}

/// lock-matrix (§6.2 L3): one exclusive advisory lock per target;
/// losers get (lock-denied, edge-deny). Outcome rows name only the
/// failing steps.
pub fn lock_matrix(vector: &Json) -> Result<SemStatus, String> {
    use std::collections::BTreeMap;
    let script = vector["inputs"]["script"].as_array().ok_or("script")?;
    let mut holders: BTreeMap<String, String> = BTreeMap::new();
    let mut denials: Vec<usize> = Vec::new();
    for (i, s) in script.iter().enumerate() {
        let actor = s["actor"].as_str().ok_or("step.actor")?;
        let action = s["action"].as_str().ok_or("step.action")?;
        let target = s["target"].as_str().unwrap_or("plane-store").to_string();
        match action {
            "acquire" => match holders.get(&target) {
                Some(h) if h != actor => denials.push(i),
                _ => {
                    holders.insert(target, actor.to_string());
                }
            },
            "release" => {
                if holders.get(&target).map(|h| h.as_str()) == Some(actor) {
                    holders.remove(&target);
                } else {
                    return Err(format!("step {i}: release without the lock"));
                }
            }
            other => return Ok(SemStatus::Unimplemented(format!("lock action {other}"))),
        }
    }
    let rows = vector["expected"]["result"]["outcomes"]
        .as_array()
        .ok_or("result.outcomes")?;
    if rows.len() != denials.len() {
        return Ok(SemStatus::Fail(format!(
            "{} denial rows expected, reducer derived {}",
            rows.len(),
            denials.len()
        )));
    }
    for (row, step) in rows.iter().zip(&denials) {
        let (Some(rs), Some(ro), Some(rd)) = (
            row["step"].as_u64(),
            row["outcome"].as_str(),
            row["disposition"].as_str(),
        ) else {
            return Err("outcome row shape".into());
        };
        if rs as usize != *step || ro != "lock-denied" || rd != "edge-deny" {
            return Ok(SemStatus::Fail(format!(
                "step {step}: expected ({ro}, {rd}) at {rs}"
            )));
        }
    }
    Ok(SemStatus::Pass)
}
