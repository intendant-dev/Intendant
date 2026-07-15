//! The per-OS portable-storage execution lane (the D-203-funded
//! `storage-macos` / `storage-linux` / `storage-windows` surfaces —
//! execution-lanes-plan.md, lane 2).
//!
//! For every storage-annotated vector this bin exercises the
//! PORTABLE file subset for real, then runs the SAME semantics the
//! CLI harness runs:
//!
//! 1. **Byte round-trip** — every hex byte-string input (streams,
//!    items, aux, rotation ops) is written to a real file in a temp
//!    dir, read back, and must reproduce byte-exactly.
//! 2. **Crash cuts as real truncations** — for `stream`+`cuts`
//!    vectors, each cut is performed with `set_len` on a fresh copy
//!    and the read-back must equal the in-memory prefix (L1's
//!    durable-prefix discipline over the OS's own truncate path).
//! 3. **The lock matrix over real OS locks** — the script's actors
//!    become PROCESSES: the first actor is this process, every other
//!    actor a spawned `--lock-agent` child; `acquire`/`release` are
//!    real `std::fs::File` advisory locks (`try_lock`/`unlock`) on
//!    per-target files, and the denial steps must match the
//!    vector's outcome rows exactly.
//! 4. **Flush + atomic replacement (review R6)** — the §13.2 cell
//!    names `framing, flush, locks, crash/corruption`, and the
//!    funded plan names portable `open/write/rename`: every stream
//!    materialization goes write-temp → `sync_all` (fsync /
//!    FlushFileBuffers) → `rename` onto the final path, so BOTH
//!    primitives are load-bearing (bypass the rename and the final
//!    path never exists — the read fails red), and end-of-run
//!    invocation counters additionally fail the lane if either
//!    primitive executed zero times.
//! 5. **Semantics** — the unmodified harness dispatch must report
//!    PASS on the vector.
//!
//! Hermetic: everything lives under a `tempfile`-style unique dir in
//! the OS temp root, removed on exit. Exit is nonzero on ANY
//! failure. This lane does NOT claim the Gate-B production concerns
//! (fsync ordering, keystores, IndexedDB failure/eviction,
//! Firefox/Safari, quota pressure) — see execution-lanes-plan.md.

use serde_json::Value as Json;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use owner_plane_reducer::harness::{plane_root, run_semantics, SemStatus};

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.is_empty()
        || !s.len().is_multiple_of(2)
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    Some(
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect(),
    )
}

/// Round-trip every hex byte-string under `inputs` through a real
/// file; count the bytes moved.
fn roundtrip_inputs(node: &Json, dir: &Path, n: &mut u32, bytes: &mut u64) -> Result<(), String> {
    match node {
        Json::Object(m) => {
            for v in m.values() {
                roundtrip_inputs(v, dir, n, bytes)?;
            }
        }
        Json::Array(a) => {
            for v in a {
                roundtrip_inputs(v, dir, n, bytes)?;
            }
        }
        Json::String(s) => {
            if let Some(raw) = unhex(s) {
                let path = dir.join(format!("input-{n}.bin"));
                *n += 1;
                *bytes += raw.len() as u64;
                std::fs::write(&path, &raw).map_err(|e| format!("write: {e}"))?;
                let back = std::fs::read(&path).map_err(|e| format!("read: {e}"))?;
                if back != raw {
                    return Err(format!("{}: read-back differs", path.display()));
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Durably materialize `bytes` at `path` through the PORTABLE
/// flush + atomic-replacement pair: write a temp sibling, fsync it
/// (`File::sync_all` — fsync on Unix, FlushFileBuffers on Windows),
/// then `rename` onto the final path. The rename is load-bearing:
/// callers read the FINAL path, so a bypassed rename fails red.
fn durable_write(path: &Path, bytes: &[u8], counters: &mut (u64, u64)) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp).map_err(|e| format!("create tmp: {e}"))?;
    std::io::Write::write_all(&mut f, bytes).map_err(|e| format!("write tmp: {e}"))?;
    f.sync_all().map_err(|e| format!("sync_all: {e}"))?;
    counters.0 += 1;
    drop(f);
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
    counters.1 += 1;
    Ok(())
}

/// Real truncation per cut: the stream lands via the durable
/// write-sync-rename path, then each cut is a `set_len` on a fresh
/// copy, read back against the in-memory prefix.
fn truncate_cuts(vector: &Json, dir: &Path, counters: &mut (u64, u64)) -> Result<u32, String> {
    let Some(stream_hex) = vector["inputs"]["stream"].as_str() else {
        return Ok(0);
    };
    let Some(cuts) = vector["inputs"]["cuts"].as_array() else {
        return Ok(0);
    };
    let stream = unhex(stream_hex).ok_or("stream hex")?;
    let full = dir.join("stream.bin");
    durable_write(&full, &stream, counters)?;
    let back = std::fs::read(&full).map_err(|e| format!("read after rename: {e}"))?;
    if back != stream {
        return Err("durable stream read-back differs".into());
    }
    let mut done = 0;
    for (i, c) in cuts.iter().enumerate() {
        let cut = c.as_u64().ok_or("cut")? as usize;
        let path = dir.join(format!("cut-{i}.bin"));
        std::fs::copy(&full, &path).map_err(|e| format!("copy: {e}"))?;
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .map_err(|e| format!("open: {e}"))?;
        f.set_len(cut as u64).map_err(|e| format!("set_len: {e}"))?;
        drop(f);
        let back = std::fs::read(&path).map_err(|e| format!("read: {e}"))?;
        if back != stream[..cut.min(stream.len())] {
            return Err(format!("cut {i}: truncated read-back differs"));
        }
        done += 1;
    }
    Ok(done)
}

/// The guarded store sibling of a lock file (`lock-X` → `data-X`).
fn data_sibling(lock_path: &str) -> PathBuf {
    let p = Path::new(lock_path);
    let name = p.file_name().unwrap_or_default().to_string_lossy();
    let data = name.replacen("lock-", "data-", 1);
    p.with_file_name(data)
}

/// One lock agent (a second real process): lines `acquire <file>` /
/// `release <file>` on stdin; replies `ok` / `denied` /
/// `readfail` (a denied loser that could NOT read the store).
fn lock_agent() -> ! {
    use std::collections::BTreeMap;
    let stdin = std::io::stdin();
    let mut held: BTreeMap<String, std::fs::File> = BTreeMap::new();
    for line in BufReader::new(stdin.lock()).lines() {
        let line = line.expect("agent stdin");
        let mut parts = line.splitn(2, ' ');
        let (cmd, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let reply = match cmd {
            "acquire" => {
                let f = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .expect("agent open");
                match f.try_lock() {
                    Ok(()) => {
                        held.insert(path.to_string(), f);
                        "ok"
                    }
                    Err(_) => {
                        // The loser stays read-only: prove the read
                        // against the guarded DATA sibling — Windows
                        // exclusive locks deny reads on the locked
                        // file itself, and the semantic is about the
                        // STORE staying readable, not the lock file.
                        let mut buf = Vec::new();
                        let ok = std::fs::File::open(data_sibling(path))
                            .and_then(|mut r| r.read_to_end(&mut buf))
                            .is_ok()
                            && buf == b"store data";
                        if ok {
                            "denied"
                        } else {
                            "readfail"
                        }
                    }
                }
            }
            "release" => {
                held.remove(path);
                "ok"
            }
            "quit" => std::process::exit(0),
            _ => "err",
        };
        println!("{reply}");
    }
    std::process::exit(0);
}

struct Agent {
    child: Child,
    out: BufReader<std::process::ChildStdout>,
}

impl Agent {
    fn spawn() -> Result<Agent, String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut child = Command::new(exe)
            .arg("--lock-agent")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawn agent: {e}"))?;
        let out = BufReader::new(child.stdout.take().expect("agent stdout"));
        Ok(Agent { child, out })
    }

    fn ask(&mut self, cmd: &str) -> Result<String, String> {
        let stdin = self.child.stdin.as_mut().expect("agent stdin");
        writeln!(stdin, "{cmd}").map_err(|e| e.to_string())?;
        stdin.flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        self.out.read_line(&mut line).map_err(|e| e.to_string())?;
        Ok(line.trim().to_string())
    }
}

/// Execute the lock-matrix script over REAL advisory locks with one
/// process per non-first actor; return the denial step indexes.
fn run_lock_script(vector: &Json, dir: &Path) -> Result<Vec<usize>, String> {
    use std::collections::BTreeMap;
    let script = vector["inputs"]["script"].as_array().ok_or("script")?;
    let mut actors: Vec<String> = Vec::new();
    for s in script {
        let a = s["actor"].as_str().ok_or("actor")?.to_string();
        if !actors.contains(&a) {
            actors.push(a);
        }
    }
    // The first actor runs in-process; the rest are real children.
    let mut agents: BTreeMap<String, Agent> = BTreeMap::new();
    for a in actors.iter().skip(1) {
        agents.insert(a.clone(), Agent::spawn()?);
    }
    let mut own: BTreeMap<String, std::fs::File> = BTreeMap::new();
    let mut denials = Vec::new();
    for (i, s) in script.iter().enumerate() {
        let actor = s["actor"].as_str().ok_or("actor")?;
        let action = s["action"].as_str().ok_or("action")?;
        let target = s["target"].as_str().unwrap_or("plane-store");
        let lock_path = dir.join(format!("lock-{target}"));
        if !lock_path.exists() {
            std::fs::write(&lock_path, b"lock target").map_err(|e| format!("lock file: {e}"))?;
            std::fs::write(dir.join(format!("data-{target}")), b"store data")
                .map_err(|e| format!("data file: {e}"))?;
        }
        let path_s = lock_path.to_string_lossy().to_string();
        let denied = if actor == actors[0] {
            match action {
                "acquire" => {
                    let f = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&lock_path)
                        .map_err(|e| format!("open: {e}"))?;
                    match f.try_lock() {
                        Ok(()) => {
                            own.insert(path_s, f);
                            false
                        }
                        Err(_) => {
                            let back = std::fs::read(data_sibling(&path_s))
                                .map_err(|e| format!("loser store read: {e}"))?;
                            if back != b"store data" {
                                return Err("loser read-only readback differs".into());
                            }
                            true
                        }
                    }
                }
                "release" => {
                    own.remove(&path_s);
                    false
                }
                other => return Err(format!("lock action {other}")),
            }
        } else {
            let agent = agents.get_mut(actor).ok_or("unknown actor")?;
            match action {
                "acquire" => match agent.ask(&format!("acquire {path_s}"))?.as_str() {
                    "ok" => false,
                    "denied" => true,
                    other => return Err(format!("agent acquire: {other}")),
                },
                "release" => {
                    agent.ask(&format!("release {path_s}"))?;
                    false
                }
                other => return Err(format!("lock action {other}")),
            }
        };
        if denied {
            denials.push(i);
        }
    }
    for (_, mut a) in agents {
        let _ = a.ask("quit");
        let _ = a.child.wait();
    }
    Ok(denials)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--lock-agent") {
        lock_agent();
    }
    if !args.is_empty() {
        eprintln!("USAGE: storage-lane        run the portable-storage execution lane");
        std::process::exit(2);
    }

    let dir = std::env::temp_dir().join(format!("owner-plane-storage-lane-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");

    let vectors_dir = plane_root().join("vectors");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&vectors_dir)
        .expect("vectors dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();

    let mut ran = 0u32;
    let mut red = 0u32;
    // (sync_all invocations, rename invocations) — the R6 proof that
    // both primitives actually executed.
    let mut durable = (0u64, 0u64);
    // The R5 manifest pin: the run set must equal the committed
    // lane manifest exactly — an annotation edit cannot silently
    // shrink this lane.
    let manifest: Json = serde_json::from_str(
        &std::fs::read_to_string(plane_root().join("coverage").join("lane-manifests.json"))
            .expect("lane-manifests.json"),
    )
    .expect("lane manifests parse");
    let required: Vec<String> = manifest["storage"]
        .as_array()
        .expect("manifest.storage")
        .iter()
        .map(|v| v.as_str().expect("manifest name").to_string())
        .collect();
    let mut executed: Vec<String> = Vec::new();
    for path in files {
        let v: Json = serde_json::from_str(&std::fs::read_to_string(&path).expect("vector read"))
            .expect("vector parse");
        let storage = v["surfaces"].as_array().is_some_and(|a| {
            a.iter()
                .any(|s| s.as_str().is_some_and(|s| s.starts_with("storage-")))
        });
        if !storage {
            continue;
        }
        ran += 1;
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        executed.push(name.clone());
        let vdir = dir.join(format!("v{ran}"));
        std::fs::create_dir_all(&vdir).expect("vector dir");

        let mut fails: Vec<String> = Vec::new();
        let (mut nfiles, mut nbytes) = (0u32, 0u64);
        if let Err(e) = roundtrip_inputs(&v["inputs"], &vdir, &mut nfiles, &mut nbytes) {
            fails.push(format!("roundtrip: {e}"));
        }
        let cuts = match truncate_cuts(&v, &vdir, &mut durable) {
            Ok(n) => n,
            Err(e) => {
                fails.push(format!("cuts: {e}"));
                0
            }
        };
        let mut locks = String::new();
        if v["case_kind"].as_str() == Some("lock-matrix") {
            match run_lock_script(&v, &vdir) {
                Ok(denials) => {
                    let want: Vec<usize> = v["expected"]["result"]["outcomes"]
                        .as_array()
                        .map(|rows| {
                            rows.iter()
                                .filter_map(|r| r["step"].as_u64().map(|s| s as usize))
                                .collect()
                        })
                        .unwrap_or_default();
                    if denials != want {
                        fails.push(format!(
                            "locks: real-process denials {denials:?} != expected {want:?}"
                        ));
                    } else {
                        locks = format!(" locks=REAL({} denial(s))", denials.len());
                    }
                }
                Err(e) => fails.push(format!("locks: {e}")),
            }
        }
        match run_semantics(&v) {
            SemStatus::Pass => {}
            SemStatus::Fail(e) => fails.push(format!("semantics: {e}")),
            SemStatus::Unimplemented(e) => fails.push(format!("semantics unimplemented: {e}")),
        }

        if fails.is_empty() {
            println!("{name:<58} files={nfiles} bytes={nbytes} cuts={cuts}{locks} PASS");
        } else {
            red += 1;
            for f in &fails {
                println!("{name:<58} FAIL: {f}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "storage lane: {ran} vector(s) executed on real files (sync_all={} rename={})",
        durable.0, durable.1
    );
    if red > 0 || ran == 0 {
        eprintln!("STORAGE LANE RED: {red} failing vector(s)");
        std::process::exit(1);
    }
    // R6 invocation proof: a run in which either portable primitive
    // never executed is red — flush and atomic replacement are part
    // of the §13.2 cell, not decoration.
    if durable.0 == 0 || durable.1 == 0 {
        eprintln!(
            "STORAGE LANE RED: flush/replacement never executed (sync_all={} rename={})",
            durable.0, durable.1
        );
        std::process::exit(1);
    }
    // R5 manifest equality, both directions.
    executed.sort();
    if executed != required {
        eprintln!(
            "STORAGE LANE RED: executed set != coverage/lane-manifests.json storage list \
             (executed {} vs required {})",
            executed.len(),
            required.len()
        );
        std::process::exit(1);
    }
}
