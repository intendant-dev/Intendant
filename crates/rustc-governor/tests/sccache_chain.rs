//! Regression tests that drive the REAL sccache binary through the real
//! governor chain (governor = rustc-wrapper, `wrap_with` = sccache)
//! against private rigs — one test per production incident:
//!
//! THE BYPASS (2026-07): the governor used to be wired as cargo's
//! `[build] rustc` with sccache as `rustc-wrapper`, so sccache treated
//! the governor as the compiler. sccache 0.15 identifies rustup proxies
//! by probing the compiler with `+stable -vV`; the governor's probe fast
//! path passed that through to the rustup proxy, so sccache classified
//! the governor AS a proxy, resolved the underlying toolchain rustc, and
//! had its SERVER invoke that binary directly for every cacheable miss —
//! ungoverned (verified live: five toolchain rustcs as sccache-server
//! children while all permits were held). Only non-cacheable work stayed
//! governed. `cacheable_miss_queues_on_the_permit_and_probe_stays_fast`
//! asserts the property that design silently lost.
//!
//! THE PERMIT LEAK (2026-07-12): see
//! `daemonized_server_does_not_inherit_the_permit`.
//!
//! THE SHAPE OF THE COMPILE IS LOAD-BEARING. sccache only routes
//! *cacheable* invocations through its server-side compile path — an rlib
//! compile with cargo-shaped args (`--crate-name … --crate-type lib
//! --emit=dep-info,link --out-dir …`). Bin/link shapes (they invoke the
//! system linker) and metadata-emit shapes are NON-cacheable: sccache runs
//! those on the client side, which was governed even under the broken
//! chain. A test built on such a shape passes against the bypass —
//! silently testing the wrong path, the exact mistake that let the
//! original bypass ship.
//!
//! Hermetic per repo doctrine: private `SCCACHE_DIR` + dedicated
//! `SCCACHE_SERVER_PORT` in tempdirs, `SCCACHE_IDLE_TIMEOUT=10` as a
//! belt-and-braces reaper for leaks, the server stopped in cleanup (Drop —
//! runs on panic too), and the only processes signalled are ones these
//! tests spawned. Skips cleanly — with an explicit message — when sccache
//! is not installed.

#![cfg(unix)]

use std::fs::File;
use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const GOVERNOR: &str = env!("CARGO_BIN_EXE_rustc-governor");
/// Deadline for bounded things (server-side compiles included) on a box
/// saturated by parallel agents.
const GENEROUS: Duration = Duration::from_secs(60);
/// How long the saturated miss is observed queued before we believe the
/// permit really gates it. Completion inside this window = the bypass
/// regressed. Negative-assertion window, so box load only makes it safer.
const SATURATION_WINDOW: Duration = Duration::from_secs(8);
/// Probes must not queue: generous for load, tiny next to a permit wait
/// that would otherwise last until the permit frees.
const PROBE_DEADLINE: Duration = Duration::from_secs(15);

// ------------------------------------------------------------ helpers ----

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn wait_deadline(child: &mut Child, deadline: Duration) -> Option<ExitStatus> {
    let t0 = Instant::now();
    while t0.elapsed() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Take LOCK_EX|LOCK_NB and keep holding it (dropping the File releases) —
/// the rig's stand-in for a running governed compile.
fn hold_exclusive(path: &Path) -> File {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    // SAFETY: `f` owns an open fd for the duration of the call.
    let locked = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 };
    assert!(locked, "test rig could not take {path:?}");
    f
}

/// Kill-and-reap on drop, so a mid-test panic never leaks a queued child.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A private sccache endpoint: own cache dir, own port, idle-timeout
/// backstop; `--stop-server` on drop. Nothing it does touches the
/// account's real sccache server (default port) or cache. Built either
/// with the server pre-started (`with_running_server`) or with the port
/// verified silent (`with_no_server` — the on-demand daemonization rig).
struct SccacheRig {
    bin: PathBuf,
    dir: PathBuf,
    port: u16,
}

impl SccacheRig {
    /// Start a private server up front (retrying ports on bind races).
    fn with_running_server(bin: &Path, dir: &Path) -> SccacheRig {
        let mut last = String::new();
        for attempt in 0..3_u32 {
            let rig = SccacheRig {
                bin: bin.to_path_buf(),
                dir: dir.to_path_buf(),
                port: candidate_port(attempt),
            };
            let mut cmd = Command::new(bin);
            cmd.arg("--start-server");
            rig.apply_env(&mut cmd);
            match cmd.output() {
                Ok(out) if out.status.success() => return rig,
                Ok(out) => {
                    last = format!(
                        "port {}: {}{}",
                        rig.port,
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
                Err(err) => panic!("could not run {}: {err}", bin.display()),
            }
        }
        panic!("could not start a private sccache server (3 ports tried); last: {last}");
    }

    /// Pick a port nothing answers on and do NOT start a server: the
    /// first governed compile's sccache client will daemonize one on
    /// demand — the exact production shape that leaked permits.
    fn with_no_server(bin: &Path, dir: &Path) -> SccacheRig {
        for attempt in 0..8_u32 {
            let port = candidate_port(attempt);
            if !tcp_port_answers(port) {
                return SccacheRig {
                    bin: bin.to_path_buf(),
                    dir: dir.to_path_buf(),
                    port,
                };
            }
        }
        panic!("could not find a silent port for the no-server rig (8 tried)");
    }

    /// Scrub every inherited SCCACHE_* var, then pin this server's env:
    /// the spawned process must talk only to the test's own server and
    /// cache, never the account's.
    fn apply_env(&self, cmd: &mut Command) {
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("SCCACHE_") {
                cmd.env_remove(key);
            }
        }
        cmd.env("SCCACHE_DIR", &self.dir)
            .env("SCCACHE_SERVER_PORT", self.port.to_string())
            // Belt and braces: even a leaked server exits after 10 idle
            // seconds.
            .env("SCCACHE_IDLE_TIMEOUT", "10");
    }
}

impl Drop for SccacheRig {
    fn drop(&mut self) {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("--stop-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        self.apply_env(&mut cmd);
        let _ = cmd.status();
    }
}

/// Random high port; no rand dependency — nanos + pid spread concurrent
/// test processes apart, `attempt` walks on a bind collision.
fn candidate_port(attempt: u32) -> u16 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let seed = nanos ^ std::process::id().wrapping_mul(2_654_435_761) ^ attempt.wrapping_mul(7_919);
    (20_000 + seed % 40_000) as u16
}

/// True iff something accepts TCP on 127.0.0.1:`port`.
fn tcp_port_answers(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(250),
    )
    .is_ok()
}

/// Spawn `governor <rustc> <cargo-shaped cacheable rlib args>` wired to the
/// rig's config and the private sccache endpoint. stderr goes to a file:
/// readable after any failure, and no pipe to deadlock an unread child.
fn governed_compile(
    server: &SccacheRig,
    config: &Path,
    rustc: &Path,
    source: &Path,
    crate_name: &str,
    out_dir: &Path,
    stderr_to: &Path,
) -> Child {
    let mut cmd = Command::new(GOVERNOR);
    cmd.arg(rustc)
        // Cargo-shaped, sccache-CACHEABLE args — see the module doc for
        // why this exact shape is load-bearing.
        .args(["--crate-name", crate_name, "--edition", "2021"])
        .arg(source)
        .args([
            "--crate-type",
            "lib",
            "--emit=dep-info,link",
            "-C",
            "debuginfo=0",
            "--out-dir",
        ])
        .arg(out_dir)
        .env("INTENDANT_GOVERNOR_CONFIG", config)
        // Hygiene: never inherit an operator kill switch into the rig.
        .env_remove("INTENDANT_GOVERNOR")
        .stdout(Stdio::null())
        .stderr(Stdio::from(File::create(stderr_to).unwrap()));
    server.apply_env(&mut cmd);
    cmd.spawn().unwrap()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

// --------------------------------------------------------------- test ----

#[test]
fn cacheable_miss_queues_on_the_permit_and_probe_stays_fast() {
    let Some(sccache) = find_on_path("sccache") else {
        eprintln!("skipped: sccache not installed");
        return;
    };
    let rustc = find_on_path("rustc").unwrap_or_else(|| PathBuf::from("rustc"));

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let cache_dir = root.join("sccache-cache");
    let permit_dir = root.join("permits");
    let out_prime = root.join("out-prime");
    let out_miss = root.join("out-miss");
    for dir in [&cache_dir, &permit_dir, &out_prime, &out_miss] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let server = SccacheRig::with_running_server(&sccache, &cache_dir);

    // One permit total; the current user classes local (impossible
    // ci_users name), so that permit is permit-local-0.
    let config = root.join("governor.toml");
    std::fs::write(
        &config,
        format!(
            "enabled = true\npermit_dir = \"{}\"\nlocal_reserved = 1\nci_reserved = 0\nci_users = [\"governor-test-no-such-user\"]\nwrap_with = \"{}\"\n",
            permit_dir.display(),
            sccache.display(),
        ),
    )
    .unwrap();

    // (1) Prime compiler detection: one governed cacheable compile with
    // the permit free. The server identifies the compiler here, so the
    // saturated phase below measures permit queueing, not detection.
    let prime_src = root.join("prime.rs");
    std::fs::write(&prime_src, "pub fn prime() {}\n").unwrap();
    let prime_err = root.join("prime.stderr");
    let mut prime = governed_compile(
        &server, &config, &rustc, &prime_src, "prime", &out_prime, &prime_err,
    );
    let status = wait_deadline(&mut prime, GENEROUS)
        .unwrap_or_else(|| panic!("prime compile did not finish: {}", read(&prime_err)));
    assert!(
        status.success(),
        "prime compile failed: {}",
        read(&prime_err)
    );
    assert!(
        out_prime.join("libprime.rlib").is_file(),
        "prime compile produced no rlib"
    );
    let log = read(&permit_dir.join("governor.log"));
    assert!(
        log.contains("permit=permit-local-0"),
        "prime compile was not governed (rig broken?): {log:?}"
    );

    // (2) Saturate: hold the single permit like a running compile, then
    // launch a UNIQUE cacheable rlib miss through the full chain. It must
    // queue on the permit — any completion inside the window means
    // cacheable server-side work escaped the pool: the production bypass,
    // regressed.
    let held = hold_exclusive(&permit_dir.join("permit-local-0"));
    let nonce = format!(
        "uniq_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let miss_src = root.join("miss.rs");
    std::fs::write(
        &miss_src,
        format!("pub const NONCE: &str = \"{nonce}\";\npub fn miss() {{}}\n"),
    )
    .unwrap();
    let miss_rlib = out_miss.join(format!("lib{nonce}.rlib"));
    let miss_err = root.join("miss.stderr");
    let mut miss = KillOnDrop(governed_compile(
        &server, &config, &rustc, &miss_src, &nonce, &out_miss, &miss_err,
    ));

    let t0 = Instant::now();
    while t0.elapsed() < SATURATION_WINDOW {
        assert!(
            miss.0.try_wait().unwrap().is_none(),
            "cacheable miss completed while the only permit was held — \
             ungoverned server-side compile: the sccache bypass regressed"
        );
        assert!(
            !miss_rlib.exists(),
            "rlib appeared while the only permit was held — the sccache bypass regressed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // (4) Probes must not queue behind the saturated pool — and they no
    // longer depend on sccache at all: the governor execs the real
    // compiler directly.
    let mut probe = Command::new(GOVERNOR)
        .arg(&rustc)
        .arg("-vV")
        .env("INTENDANT_GOVERNOR_CONFIG", &config)
        .env_remove("INTENDANT_GOVERNOR")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let probe_status = wait_deadline(&mut probe, PROBE_DEADLINE)
        .expect("-vV probe queued behind a held permit (probe fast path broken)");
    assert!(probe_status.success());
    let mut version = String::new();
    probe
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut version)
        .unwrap();
    assert!(
        version.contains("rustc"),
        "probe output is not a rustc -vV banner: {version:?}"
    );
    assert!(
        miss.0.try_wait().unwrap().is_none(),
        "miss must still be queued after the probe"
    );

    // (3) Release: the queued miss acquires the permit, compiles
    // server-side, and lands its rlib.
    drop(held);
    let status = wait_deadline(&mut miss.0, GENEROUS)
        .unwrap_or_else(|| panic!("released miss never completed: {}", read(&miss_err)));
    assert!(
        status.success(),
        "released miss failed: {}",
        read(&miss_err)
    );
    assert!(miss_rlib.is_file(), "miss produced no rlib after release");
}

/// Regression test for THE production permit leak (2026-07-12): the old
/// governed path cleared FD_CLOEXEC on the permit fd and exec(2)'d the
/// sccache client — and when no server was listening, the client
/// daemonized one, so the long-lived server (ppid 1) inherited the
/// permit fd through client → server fd inheritance. flock(2) locks
/// belong to the open file description, so the permit stayed held after
/// the client exited, for the server's whole lifetime: a 3-permit pool
/// silently ran as 2 for hours (verified live — a local sccache server
/// held permit-ci-1 on fd 9; killing it released the permit).
///
/// The fix is parent-held permits: the governor keeps the fd (CLOEXEC
/// set, invisible to every child), spawns the chain, and waits. This
/// test reproduces the daemonization shape — NO pre-started server, one
/// governed CACHEABLE compile whose client spins the server up on
/// demand — and hard-asserts the permit is immediately lockable the
/// moment the governor exits. Best-effort second half: confirm the
/// daemonized server exists and (where lsof is available) holds no fd
/// on the permit file.
#[test]
fn daemonized_server_does_not_inherit_the_permit() {
    let Some(sccache) = find_on_path("sccache") else {
        eprintln!("skipped: sccache not installed");
        return;
    };
    let rustc = find_on_path("rustc").unwrap_or_else(|| PathBuf::from("rustc"));

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let cache_dir = root.join("sccache-cache");
    let permit_dir = root.join("permits");
    let out_dir = root.join("out");
    for dir in [&cache_dir, &permit_dir, &out_dir] {
        std::fs::create_dir_all(dir).unwrap();
    }
    // NO server pre-started — the rig verified the port is silent, so the
    // governed compile below is what daemonizes one.
    let rig = SccacheRig::with_no_server(&sccache, &cache_dir);
    assert!(
        !tcp_port_answers(rig.port),
        "rig port must be silent before the governed compile"
    );

    // One permit total; the current user classes local (impossible
    // ci_users name), so that permit is permit-local-0.
    let config = root.join("governor.toml");
    std::fs::write(
        &config,
        format!(
            "enabled = true\npermit_dir = \"{}\"\nlocal_reserved = 1\nci_reserved = 0\nci_users = [\"governor-test-no-such-user\"]\nwrap_with = \"{}\"\n",
            permit_dir.display(),
            sccache.display(),
        ),
    )
    .unwrap();

    // One governed CACHEABLE compile (the shape is load-bearing — module
    // doc): its client finds no server and daemonizes one mid-compile.
    let src = root.join("leak.rs");
    std::fs::write(&src, "pub fn leak_probe() {}\n").unwrap();
    let stderr = root.join("leak.stderr");
    let mut compile =
        governed_compile(&rig, &config, &rustc, &src, "leak_probe", &out_dir, &stderr);
    let status = wait_deadline(&mut compile, GENEROUS)
        .unwrap_or_else(|| panic!("compile did not finish: {}", read(&stderr)));
    assert!(status.success(), "compile failed: {}", read(&stderr));
    assert!(
        out_dir.join("libleak_probe.rlib").is_file(),
        "compile produced no rlib"
    );
    // The run really was governed — a fail-open run holds no permit and
    // would pass the leak assert vacuously.
    let log = read(&permit_dir.join("governor.log"));
    assert!(
        log.contains("permit=permit-local-0"),
        "compile was not governed (rig broken?): {log:?}"
    );

    // THE hard assert: the governor is gone, so the permit must be
    // immediately lockable. Under the leak, the daemonized server —
    // ppid 1, lifetime unbounded next to a compile — still holds the
    // flock right here.
    let permit = File::open(permit_dir.join("permit-local-0")).unwrap();
    // SAFETY: `permit` owns an open fd for the duration of the call.
    let lockable = unsafe { libc::flock(permit.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 };
    assert!(
        lockable,
        "permit still flocked after the governed compile exited — a child of the \
         governed chain (the daemonized sccache server) inherited the permit fd: \
         the 2026-07-12 permit leak regressed"
    );
    // SAFETY: as above; LOCK_UN releases the probe lock this test took.
    unsafe {
        libc::flock(permit.as_raw_fd(), libc::LOCK_UN);
    }

    // Best-effort half: the daemonization really happened (a server now
    // answers stats on the private port)…
    let mut stats = Command::new(&sccache);
    stats
        .arg("--show-stats")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    rig.apply_env(&mut stats);
    let server_up = stats.status().map(|s| s.success()).unwrap_or(false);
    assert!(
        server_up,
        "no daemonized sccache server answering on the private port — the rig did \
         not exercise the on-demand daemonization path this regression is about"
    );
    // …and, where lsof exists, that server holds no fd on the permit file.
    if let Some(lsof) = find_on_path("lsof") {
        match pid_listening_on(&lsof, rig.port) {
            Some(pid) => {
                let out = Command::new(&lsof)
                    .args(["-p", &pid.to_string()])
                    .output()
                    .expect("run lsof -p");
                let table = String::from_utf8_lossy(&out.stdout);
                let permit_path = permit_dir.join("permit-local-0").display().to_string();
                assert!(
                    !table.contains(&permit_path),
                    "daemonized sccache server (pid {pid}) holds an fd on the permit file:\n{table}"
                );
            }
            None => eprintln!("lsof found no listener pid; skipping the server fd scan"),
        }
    } else {
        eprintln!("lsof not installed; skipping the server fd scan");
    }
    // rig Drop stops the daemonized server.
}

/// Pid listening on 127.0.0.1:`port`, via `lsof -t` (best-effort).
fn pid_listening_on(lsof: &Path, port: u16) -> Option<i32> {
    let out = Command::new(lsof)
        .args(["-t", "-n", "-P", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}
