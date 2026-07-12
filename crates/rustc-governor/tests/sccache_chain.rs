//! Regression test for THE production bypass (2026-07): the governor used
//! to be wired as cargo's `[build] rustc` with sccache as `rustc-wrapper`,
//! so sccache treated the governor as the compiler. sccache 0.15
//! identifies rustup proxies by probing the compiler with `+stable -vV`;
//! the governor's probe fast path passed that through to the rustup proxy,
//! so sccache classified the governor AS a proxy, resolved the underlying
//! toolchain rustc, and had its SERVER invoke that binary directly for
//! every cacheable miss — ungoverned (verified live: five toolchain rustcs
//! as sccache-server children while all permits were held). Only
//! non-cacheable work stayed governed.
//!
//! This test drives the REAL sccache binary through the new chain
//! (governor = rustc-wrapper, `wrap_with` = sccache) against a private
//! server and asserts the property the old chain silently lost: with the
//! single permit held, a CACHEABLE rlib miss must NOT complete — it queues
//! on the permit — while a `-vV` probe still completes promptly.
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
//! runs on panic too), and the only processes signalled are ones this test
//! spawned. Skips cleanly — with an explicit message — when sccache is not
//! installed.

#![cfg(unix)]

use std::fs::File;
use std::io::Read;
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

/// A private sccache server: own cache dir, own port, idle-timeout
/// backstop; stopped on drop. Nothing it does touches the account's real
/// sccache server (default port) or cache.
struct SccacheServer {
    bin: PathBuf,
    dir: PathBuf,
    port: u16,
}

impl SccacheServer {
    fn start(bin: &Path, dir: &Path) -> SccacheServer {
        let mut last = String::new();
        for attempt in 0..3_u32 {
            let server = SccacheServer {
                bin: bin.to_path_buf(),
                dir: dir.to_path_buf(),
                port: candidate_port(attempt),
            };
            let mut cmd = Command::new(bin);
            cmd.arg("--start-server");
            server.apply_env(&mut cmd);
            match cmd.output() {
                Ok(out) if out.status.success() => return server,
                Ok(out) => {
                    last = format!(
                        "port {}: {}{}",
                        server.port,
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
                Err(err) => panic!("could not run {}: {err}", bin.display()),
            }
        }
        panic!("could not start a private sccache server (3 ports tried); last: {last}");
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

impl Drop for SccacheServer {
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

/// Spawn `governor <rustc> <cargo-shaped cacheable rlib args>` wired to the
/// rig's config and the private sccache server. stderr goes to a file:
/// readable after any failure, and no pipe to deadlock an unread child.
fn governed_compile(
    server: &SccacheServer,
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
    let server = SccacheServer::start(&sccache, &cache_dir);

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
