//! Acceptance battery for rustc-governor: spawns the real governor binary
//! (`CARGO_BIN_EXE_rustc-governor` — the standard mechanism, which is why
//! this crate carries an integration-test file rather than the repo's usual
//! inline-only tests: a unit test inside the bin target links the libtest
//! harness `main`, so there is no governor binary to exec) against hermetic
//! tempdir rigs wired through `INTENDANT_GOVERNOR_CONFIG`.
//!
//! The governor is cargo's RUSTC_WRAPPER, so every spawn follows the
//! wrapper argv contract: argv[1] is the "real compiler" (a /bin/sh
//! fixture the rig provides), argv[2..] are its args. No machine state is
//! read or mutated: every permit dir, config, marker, and fixture lives in
//! the test's own tempdir, and the only processes signalled are the ones a
//! test itself spawned.
//!
//! Loaded-box tolerance: assertions use generous deadlines (`GENEROUS`) and
//! never assert that something happened *fast* — only that invariants hold
//! (ceilings, gates) and that bounded things complete.

#![cfg(unix)]

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const GOVERNOR: &str = env!("CARGO_BIN_EXE_rustc-governor");
/// Deadline for anything that should happen "promptly" — sized for a box
/// saturated by parallel agents, not for the typical millisecond case.
const GENEROUS: Duration = Duration::from_secs(15);
/// How long a should-be-blocked governor is observed before we believe it
/// is really waiting (several 100ms poll ticks plus load headroom).
const BLOCKED_OBSERVATION: Duration = Duration::from_millis(1200);

// ---------------------------------------------------------------- rig ----

struct Rig {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    permit_dir: PathBuf,
}

impl Rig {
    fn new() -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let permit_dir = root.join("permits");
        std::fs::create_dir_all(&permit_dir).unwrap();
        Rig {
            _tmp: tmp,
            root,
            permit_dir,
        }
    }

    /// Write a config classing the current user per `ci_user_is_me`:
    /// the tests never depend on which account actually runs them (CI
    /// fleet accounts are literally named "ci"), so "local" configs list
    /// an impossible username and "ci" configs list the real one.
    fn config(
        &self,
        name: &str,
        local: u32,
        ci: u32,
        ci_user_is_me: bool,
        enabled: bool,
    ) -> PathBuf {
        self.config_inner(name, local, ci, ci_user_is_me, enabled, None)
    }

    /// Same, plus a `wrap_with` chain front (the sccache stand-in).
    fn config_with_wrap(
        &self,
        name: &str,
        local: u32,
        ci: u32,
        ci_user_is_me: bool,
        enabled: bool,
        wrap: &Path,
    ) -> PathBuf {
        self.config_inner(name, local, ci, ci_user_is_me, enabled, Some(wrap))
    }

    fn config_inner(
        &self,
        name: &str,
        local: u32,
        ci: u32,
        ci_user_is_me: bool,
        enabled: bool,
        wrap: Option<&Path>,
    ) -> PathBuf {
        let ci_users = if ci_user_is_me {
            format!("[\"{}\"]", me())
        } else {
            "[\"governor-test-no-such-user\"]".to_string()
        };
        let mut text = format!(
            "enabled = {enabled}\npermit_dir = \"{}\"\nlocal_reserved = {local}\nci_reserved = {ci}\nci_users = {ci_users}\n",
            self.permit_dir.display(),
        );
        if let Some(wrap) = wrap {
            text.push_str(&format!("wrap_with = \"{}\"\n", wrap.display()));
        }
        let path = self.root.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn permit(&self, name: &str) -> PathBuf {
        self.permit_dir.join(name)
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.permit_dir.join("governor.log")).unwrap_or_default()
    }
}

fn me() -> String {
    let out = Command::new("id").arg("-un").output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Spawn the governor under the RUSTC_WRAPPER contract: `real` becomes
/// argv[1] (the compiler), `args` become argv[2..] (the compiler's args).
fn spawn_governor(config: &Path, real: &Path, args: &[&str], envs: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(GOVERNOR);
    cmd.arg(real)
        .args(args)
        .env("INTENDANT_GOVERNOR_CONFIG", config)
        // Hygiene: never inherit an operator kill switch into the rig.
        .env_remove("INTENDANT_GOVERNOR")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.spawn().unwrap()
}

// ------------------------------------------------------- flock helpers ----

fn open_rw(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap()
}

fn flock_nb(file: &File, op: libc::c_int) -> bool {
    // SAFETY: `file` owns an open fd for the duration of the call.
    unsafe { libc::flock(file.as_raw_fd(), op) == 0 }
}

/// Take LOCK_EX|LOCK_NB and keep holding it (dropping the File releases).
fn hold_exclusive(path: &Path) -> File {
    let f = open_rw(path);
    assert!(
        flock_nb(&f, libc::LOCK_EX | libc::LOCK_NB),
        "test rig could not take {path:?}"
    );
    f
}

/// Register demand the way a waiter does: LOCK_SH held until dropped.
fn hold_shared(path: &Path) -> File {
    let f = open_rw(path);
    assert!(flock_nb(&f, libc::LOCK_SH | libc::LOCK_NB));
    f
}

/// True iff someone currently holds the file exclusively (probe + release).
fn is_exclusively_locked(path: &Path) -> bool {
    let f = open_rw(path);
    if flock_nb(&f, libc::LOCK_EX | libc::LOCK_NB) {
        flock_nb(&f, libc::LOCK_UN);
        false
    } else {
        true
    }
}

// ------------------------------------------------------- wait helpers ----

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

/// Assert the child keeps running for the whole observation window.
fn assert_still_running_for(child: &mut Child, window: Duration) {
    let t0 = Instant::now();
    while t0.elapsed() < window {
        assert!(
            child.try_wait().unwrap().is_none(),
            "process exited while it should have been waiting for a permit"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for<F: FnMut() -> bool>(mut cond: F, deadline: Duration, what: &str) {
    let t0 = Instant::now();
    while t0.elapsed() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

fn kill_and_reap(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

// ------------------------------------------------------------- tests ----

/// (a) Global ceiling: with local=1 + ci=2, eight concurrent governed
/// invocations never exceed three simultaneous holders. Concurrency is
/// measured from the fixture's own start/end event stream (atomic O_APPEND
/// lines), not wall clocks, so load can stretch time without lying to us.
#[test]
fn global_ceiling_bounds_concurrent_holders() {
    let rig = Rig::new();
    let events = rig.root.join("events");
    let script = rig.script(
        "job.sh",
        &format!(
            "echo start >> {ev}\nsleep 0.4\necho end >> {ev}",
            ev = events.display()
        ),
    );
    let config = rig.config("gov.toml", 1, 2, false, true);

    let mut kids: Vec<Child> = (0..8)
        .map(|_| spawn_governor(&config, &script, &[], &[]))
        .collect();
    let overall = Instant::now();
    for child in &mut kids {
        let left = Duration::from_secs(45)
            .checked_sub(overall.elapsed())
            .unwrap_or_default();
        let status = wait_deadline(child, left).expect("governed job must finish");
        assert!(status.success());
    }

    let text = std::fs::read_to_string(&events).unwrap();
    let (mut current, mut max, mut starts, mut ends) = (0_i32, 0_i32, 0, 0);
    for line in text.lines() {
        match line {
            "start" => {
                current += 1;
                starts += 1;
                max = max.max(current);
            }
            "end" => {
                current -= 1;
                ends += 1;
            }
            other => panic!("unexpected event line {other:?}"),
        }
    }
    assert_eq!((starts, ends), (8, 8));
    assert!(
        max <= 3,
        "ceiling violated: {max} concurrent compile holders (total permits = 3)"
    );
}

/// (b) Idle borrowing: with the local reservation exhausted and no CI
/// demand registered, a local invocation takes a CI-reserved permit.
#[test]
fn idle_borrowing_takes_foreign_permit() {
    let rig = Rig::new();
    let marker = rig.root.join("marker");
    let script = rig.script("done.sh", &format!("echo done >> {}", marker.display()));
    let config = rig.config("gov.toml", 1, 2, false, true);

    let _local_held = hold_exclusive(&rig.permit("permit-local-0"));
    let mut child = spawn_governor(&config, &script, &[], &[]);
    let status = wait_deadline(&mut child, GENEROUS).expect("borrower must not wait");
    assert!(status.success());
    assert!(marker.exists());
    let log = rig.log();
    let line = log.lines().last().unwrap_or_default();
    assert!(
        line.contains("class=local") && line.contains("permit=permit-ci-"),
        "expected a borrowed CI permit in the log, got: {line:?}"
    );
}

/// (c) Contested split, local side: with CI waiters registered (LOCK_SH on
/// demand-ci), a local invocation must NOT take a free CI-reserved permit —
/// it waits for its own class.
#[test]
fn contested_ci_reserve_is_not_borrowed_by_local() {
    let rig = Rig::new();
    let marker = rig.root.join("marker");
    let script = rig.script("done.sh", &format!("echo done >> {}", marker.display()));
    let config = rig.config("gov.toml", 1, 2, false, true);

    let local_held = hold_exclusive(&rig.permit("permit-local-0"));
    // Pre-create the CI permits so their freeness is observable.
    let (ci0, ci1) = (rig.permit("permit-ci-0"), rig.permit("permit-ci-1"));
    let (_c0, _c1) = (open_rw(&ci0), open_rw(&ci1));
    let _ci_waiters = hold_shared(&rig.permit("demand-ci"));

    let mut child = spawn_governor(&config, &script, &[], &[]);
    assert_still_running_for(&mut child, BLOCKED_OBSERVATION);
    assert!(
        !marker.exists(),
        "governed job ran while it should be waiting"
    );
    assert!(
        !is_exclusively_locked(&ci0) && !is_exclusively_locked(&ci1),
        "local invocation touched a contested CI reserve"
    );

    drop(local_held);
    let status = wait_deadline(&mut child, GENEROUS).expect("waiter must acquire after release");
    assert!(status.success());
    assert!(marker.exists());
    let log = rig.log();
    assert!(
        log.lines()
            .last()
            .unwrap_or_default()
            .contains("permit=permit-local-0"),
        "must have won its own class's permit: {log:?}"
    );
}

/// (c) Contested split, CI side: symmetric — local demand protects the
/// local reservation from CI borrowers.
#[test]
fn contested_local_reserve_is_not_borrowed_by_ci() {
    let rig = Rig::new();
    let marker = rig.root.join("marker");
    let script = rig.script("done.sh", &format!("echo done >> {}", marker.display()));
    let config = rig.config("gov.toml", 1, 2, true, true);

    let ci_held_0 = hold_exclusive(&rig.permit("permit-ci-0"));
    let _ci_held_1 = hold_exclusive(&rig.permit("permit-ci-1"));
    let local0 = rig.permit("permit-local-0");
    let _l0 = open_rw(&local0);
    let _local_waiters = hold_shared(&rig.permit("demand-local"));

    let mut child = spawn_governor(&config, &script, &[], &[]);
    assert_still_running_for(&mut child, BLOCKED_OBSERVATION);
    assert!(!marker.exists());
    assert!(
        !is_exclusively_locked(&local0),
        "CI invocation touched a contested local reserve"
    );

    drop(ci_held_0);
    let status = wait_deadline(&mut child, GENEROUS).expect("waiter must acquire after release");
    assert!(status.success());
    let log = rig.log();
    let line = log.lines().last().unwrap_or_default();
    assert!(
        line.contains("class=ci") && line.contains("permit=permit-ci-"),
        "must have won a CI permit: {line:?}"
    );
}

/// (d) No starvation: a waiter acquires within a bounded time of permits
/// freeing.
#[test]
fn waiter_acquires_promptly_after_release() {
    let rig = Rig::new();
    let marker = rig.root.join("marker");
    let script = rig.script("done.sh", &format!("echo done >> {}", marker.display()));
    let config = rig.config("gov.toml", 1, 0, false, true);

    let held = hold_exclusive(&rig.permit("permit-local-0"));
    let mut child = spawn_governor(&config, &script, &[], &[]);
    assert_still_running_for(&mut child, Duration::from_millis(400));
    drop(held);
    let status = wait_deadline(&mut child, GENEROUS).expect("waiter starved");
    assert!(status.success());
    let log = rig.log();
    let line = log.lines().last().unwrap_or_default();
    assert!(
        line.contains("wait_ms="),
        "log must carry the wait: {line:?}"
    );
}

/// (e) Crash release: SIGKILL the GOVERNOR — the permit holder — and the
/// flock evaporates with it: the permit is immediately acquirable even
/// though the governed child is still running (it never held the fd;
/// FD_CLOEXEC stays set). The orphaned child finishing its compile
/// momentarily ungoverned is the documented crash semantics — SIGKILL
/// cannot be forwarded. The fixture reports its pid so the test can reap
/// the orphan it deliberately creates.
#[test]
fn sigkilled_governor_releases_its_permit() {
    let rig = Rig::new();
    let ready = rig.root.join("ready");
    let script = rig.script(
        "hold.sh",
        &format!(
            "echo $$ >> {}\nwhile :; do sleep 0.05; done",
            ready.display()
        ),
    );
    let config = rig.config("gov.toml", 1, 0, false, true);
    let permit = rig.permit("permit-local-0");

    let mut child = spawn_governor(&config, &script, &[], &[]);
    // The fixture's pid line (`$$` + newline) doubles as the ready signal.
    wait_for(
        || {
            std::fs::read_to_string(&ready)
                .map(|s| s.ends_with('\n'))
                .unwrap_or(false)
        },
        GENEROUS,
        "holder to start",
    );
    assert!(is_exclusively_locked(&permit));
    child.kill().unwrap();
    child.wait().unwrap();
    wait_for(
        || !is_exclusively_locked(&permit),
        GENEROUS,
        "permit release after SIGKILL",
    );

    // Reap the orphaned fixture (still looping — crash semantics) so the
    // test leaks no process.
    let orphan: libc::pid_t = std::fs::read_to_string(&ready)
        .unwrap()
        .trim()
        .parse()
        .expect("fixture wrote its pid");
    // SAFETY: the pid was reported by the fixture this test (transitively)
    // spawned; kill(2) takes only the pid and signal number.
    unsafe {
        libc::kill(orphan, libc::SIGKILL);
    }

    // And a fresh governed invocation can take it end to end.
    let marker = rig.root.join("marker");
    let quick = rig.script("done.sh", &format!("echo done >> {}", marker.display()));
    let config2 = rig.config("gov2.toml", 1, 0, false, true);
    let mut second = spawn_governor(&config2, &quick, &[], &[]);
    assert!(wait_deadline(&mut second, GENEROUS).unwrap().success());
    assert!(marker.exists());
}

/// (f) Parent-held permit: while the governed child runs, the permit is
/// held — by the governor itself, which stays alive as the child's
/// parent (the fd keeps FD_CLOEXEC, so the child never holds it) — and
/// it frees when the chain exits. The exec-era ancestor of this test
/// (`permit_lock_survives_exec`) pinned the opposite fd story — the
/// flock riding a FD_CLOEXEC-cleared fd through exec(2) — which is the
/// design that leaked permits into daemonized sccache servers (see
/// tests/sccache_chain.rs).
#[test]
fn permit_held_while_child_runs_and_freed_on_exit() {
    let rig = Rig::new();
    let ready = rig.root.join("ready");
    let stop = rig.root.join("stop");
    let script = rig.script(
        "until_stop.sh",
        &format!(
            "echo ready >> {r}\nwhile [ ! -e {s} ]; do sleep 0.05; done",
            r = ready.display(),
            s = stop.display()
        ),
    );
    let config = rig.config("gov.toml", 1, 0, false, true);
    let permit = rig.permit("permit-local-0");

    let mut child = spawn_governor(&config, &script, &[], &[]);
    wait_for(|| ready.exists(), GENEROUS, "governed fixture to start");
    // The fixture (the governor's child) is running: the waiting governor
    // parent must be holding the permit.
    assert!(
        is_exclusively_locked(&permit),
        "permit not held while the governed child runs"
    );
    std::fs::write(&stop, b"").unwrap();
    assert!(wait_deadline(&mut child, GENEROUS).unwrap().success());
    wait_for(
        || !is_exclusively_locked(&permit),
        GENEROUS,
        "permit release on exit",
    );
}

/// (g) Exit status propagates through the governor's wait.
#[test]
fn exit_status_propagates() {
    let rig = Rig::new();
    let script = rig.script("exit42.sh", "exit 42");
    let config = rig.config("gov.toml", 1, 0, false, true);
    let mut child = spawn_governor(&config, &script, &[], &[]);
    let status = wait_deadline(&mut child, GENEROUS).unwrap();
    assert_eq!(status.code(), Some(42));
}

/// (g) Signal forwarding + disposition: SIGTERM to the governor is
/// forwarded to the governed child (the governor is the permit-holding
/// parent, so a signal aimed at the wrapper pid must reach the real
/// work), and once the child dies of it the governor re-raises the same
/// signal on itself — cargo observes the identical signal death the old
/// exec design produced by construction. The tick stream proves the
/// child really died of the forwarded signal: a governor that died alone
/// would orphan the fixture, still ticking. (The fixture self-bounds at
/// ~30s so a regression can't leak an infinite loop.)
#[test]
fn sigterm_forwards_to_child_and_signal_death_propagates() {
    let rig = Rig::new();
    let ticks = rig.root.join("ticks");
    let script = rig.script(
        "tick.sh",
        &format!(
            "i=0\nwhile [ $i -lt 600 ]; do echo tick >> {}; i=$((i+1)); sleep 0.05; done",
            ticks.display()
        ),
    );
    let config = rig.config("gov.toml", 1, 0, false, true);
    let mut child = spawn_governor(&config, &script, &[], &[]);
    wait_for(|| ticks.exists(), GENEROUS, "fixture to start ticking");
    // SAFETY: pid was spawned by this test and not yet reaped; kill(2)
    // takes only the pid and signal number.
    let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(rc, 0);
    let status = wait_deadline(&mut child, GENEROUS).unwrap();
    assert_eq!(status.signal(), Some(libc::SIGTERM));
    // The child must be dead too: its tick stream stops growing. (Settle
    // beat first, then a window several fixture periods long.)
    std::thread::sleep(Duration::from_millis(200));
    let after_settle = std::fs::metadata(&ticks).unwrap().len();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        std::fs::metadata(&ticks).unwrap().len(),
        after_settle,
        "tick file still growing: the governed child survived the forwarded SIGTERM"
    );
    // And the permit came back with the governor's exit.
    wait_for(
        || !is_exclusively_locked(&rig.permit("permit-local-0")),
        GENEROUS,
        "permit release after signal death",
    );
}

/// (h) Probe fast path: version and pure --print invocations complete while
/// every permit is held AND both classes have registered waiters; a real
/// compile carrying --print does not.
#[test]
fn probe_fast_path_ignores_exhausted_pool() {
    let rig = Rig::new();
    let marker = rig.root.join("marker");
    let script = rig.script("probe.sh", &format!("echo probed >> {}", marker.display()));
    let config = rig.config("gov.toml", 1, 2, false, true);

    let _p0 = hold_exclusive(&rig.permit("permit-local-0"));
    let _p1 = hold_exclusive(&rig.permit("permit-ci-0"));
    let _p2 = hold_exclusive(&rig.permit("permit-ci-1"));
    let _d0 = hold_shared(&rig.permit("demand-local"));
    let _d1 = hold_shared(&rig.permit("demand-ci"));

    let mut version = spawn_governor(&config, &script, &["-vV"], &[]);
    assert!(wait_deadline(&mut version, GENEROUS).unwrap().success());
    let mut print = spawn_governor(&config, &script, &["--print", "cfg"], &[]);
    assert!(wait_deadline(&mut print, GENEROUS).unwrap().success());
    let text = std::fs::read_to_string(&marker).unwrap();
    assert_eq!(text.lines().count(), 2, "both probes must have exec'd");

    // Negative: a compile that merely carries --print is governed — it must
    // wait behind the exhausted pool, not bypass.
    let mut governed = spawn_governor(
        &config,
        &script,
        &[
            "--print",
            "native-static-libs",
            "--emit=metadata,link",
            "lib.rs",
        ],
        &[],
    );
    assert_still_running_for(&mut governed, BLOCKED_OBSERVATION);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap().lines().count(),
        2,
        "a governed compile bypassed the pool"
    );
    kill_and_reap(governed);
}

/// (i) Live kill switch: flipping `enabled = false` mid-flight makes the
/// next invocation bypass immediately — and unwedges in-flight waiters
/// within a poll tick (both fail open, permits untouched).
#[test]
fn kill_switch_flips_live() {
    let rig = Rig::new();
    let marker = rig.root.join("marker");
    let script = rig.script("done.sh", &format!("echo done >> {}", marker.display()));
    let config = rig.config("gov.toml", 1, 0, false, true);

    let _held = hold_exclusive(&rig.permit("permit-local-0"));
    let mut waiter = spawn_governor(&config, &script, &[], &[]);
    assert_still_running_for(&mut waiter, Duration::from_millis(600));

    // Flip the switch in place (same path the running waiter re-reads).
    rig.config("gov.toml", 1, 0, false, false);

    let status = wait_deadline(&mut waiter, GENEROUS).expect("waiter must unwedge, fail-open");
    assert!(status.success());
    let mut next = spawn_governor(&config, &script, &[], &[]);
    assert!(wait_deadline(&mut next, GENEROUS).unwrap().success());
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 2);
}

// ------------------------------------------------ wrapper chain shape ----

/// Governed chain shape: with `wrap_with` configured, a governed
/// invocation acquires its permit, then spawns `wrap_with <real>
/// <args…>` and waits — argv order is the sccache client contract
/// (argv[1] = the compiler, the rest its args, exactly as cargo handed
/// them to the governor).
#[test]
fn governed_invocation_runs_wrap_chain() {
    let rig = Rig::new();
    let real_marker = rig.root.join("real");
    let wrap_marker = rig.root.join("wrap");
    let real = rig.script(
        "rustc.sh",
        &format!("echo \"real $*\" >> {}", real_marker.display()),
    );
    // Log the argv the wrap step received, then run it — what the sccache
    // client does with its compiler argv.
    let wrap = rig.script(
        "wrap.sh",
        &format!("echo \"$*\" >> {}\nexec \"$@\"", wrap_marker.display()),
    );
    let config = rig.config_with_wrap("gov.toml", 1, 0, false, true, &wrap);

    let mut child = spawn_governor(&config, &real, &["--crate-name", "x", "lib.rs"], &[]);
    assert!(wait_deadline(&mut child, GENEROUS).unwrap().success());

    let wrap_line = std::fs::read_to_string(&wrap_marker).unwrap();
    assert_eq!(
        wrap_line.trim_end(),
        format!("{} --crate-name x lib.rs", real.display()),
        "wrap front must receive <real> <args…> verbatim"
    );
    let real_line = std::fs::read_to_string(&real_marker).unwrap();
    assert_eq!(real_line.trim_end(), "real --crate-name x lib.rs");
    // And the run really was governed: exactly the permit path, logged.
    assert!(
        rig.log().contains("permit=permit-local-0"),
        "governed wrap-chain run must hold a permit: {:?}",
        rig.log()
    );
}

/// Fail-open paths keep the wrap chain: a disabled governor must behave
/// exactly like a plain sccache rustc-wrapper — caching is never dropped.
/// Covers the config kill switch, the env override, and zero configured
/// permits. (Missing/unparseable config cannot know `wrap_with` and runs
/// the compiler directly — covered by the missing/unparseable tests
/// below.)
#[test]
fn fail_open_paths_still_exec_wrap_chain() {
    struct Case {
        name: &'static str,
        enabled: bool,
        local: u32,
        envs: &'static [(&'static str, &'static str)],
    }
    for case in [
        Case {
            name: "kill-switch",
            enabled: false,
            local: 1,
            envs: &[],
        },
        Case {
            name: "env-off",
            enabled: true,
            local: 1,
            envs: &[("INTENDANT_GOVERNOR", "off")],
        },
        Case {
            name: "zero-permits",
            enabled: true,
            local: 0,
            envs: &[],
        },
    ] {
        let rig = Rig::new();
        let real_marker = rig.root.join("real");
        let wrap_marker = rig.root.join("wrap");
        let real = rig.script(
            "rustc.sh",
            &format!("echo real >> {}", real_marker.display()),
        );
        let wrap = rig.script(
            "wrap.sh",
            &format!("echo wrap >> {}\nexec \"$@\"", wrap_marker.display()),
        );
        let config = rig.config_with_wrap("gov.toml", case.local, 0, false, case.enabled, &wrap);
        // Where a permit exists, hold it: fail-open must not wait on it.
        let _held = (case.local > 0).then(|| hold_exclusive(&rig.permit("permit-local-0")));

        let mut child = spawn_governor(&config, &real, &[], case.envs);
        let status = wait_deadline(&mut child, GENEROUS)
            .unwrap_or_else(|| panic!("[{}] fail-open run must complete", case.name));
        assert!(status.success(), "[{}] chain must succeed", case.name);
        assert!(
            wrap_marker.exists(),
            "[{}] fail-open dropped the wrap_with chain (caching lost)",
            case.name
        );
        assert!(
            real_marker.exists(),
            "[{}] the real compiler never ran",
            case.name
        );
        // Ungoverned runs are silent in the log (doctrine: the log answers
        // "who waited on which permit").
        assert_eq!(rig.log(), "", "[{}] fail-open must not log", case.name);
    }
}

/// `wrap_with` pointing at a path that doesn't exist must not break the
/// build: the wrap spawn fails, and the governor falls back to running
/// the compiler directly — still governed (the permit was already held,
/// and stays parent-held for the fallback child too).
#[test]
fn missing_wrap_with_falls_back_to_direct_run() {
    let rig = Rig::new();
    let marker = rig.root.join("marker");
    let real = rig.script("rustc.sh", &format!("echo real >> {}", marker.display()));
    let config = rig.config_with_wrap(
        "gov.toml",
        1,
        0,
        false,
        true,
        Path::new("/nonexistent/rustc-governor-test/sccache"),
    );
    let mut child = spawn_governor(&config, &real, &["--crate-name", "x"], &[]);
    assert!(wait_deadline(&mut child, GENEROUS).unwrap().success());
    assert!(marker.exists(), "fallback must still run the compiler");
    assert!(
        rig.log().contains("permit=permit-local-0"),
        "fallback run stays governed: {:?}",
        rig.log()
    );
}

/// `wrap_with` pointing back at the governor binary would run an
/// identical invocation forever. It is config-file state, so it fails
/// OPEN: the chain front is ignored and the compiler runs directly.
#[test]
fn wrap_with_pointing_at_governor_falls_back_to_direct_run() {
    let rig = Rig::new();
    let marker = rig.root.join("marker");
    let real = rig.script("rustc.sh", &format!("echo real >> {}", marker.display()));
    let config = rig.config_with_wrap("gov.toml", 1, 0, false, true, Path::new(GOVERNOR));
    let mut child = spawn_governor(&config, &real, &["--crate-name", "x"], &[]);
    let status =
        wait_deadline(&mut child, GENEROUS).expect("self-wrap must fall back, not loop or wait");
    assert!(status.success());
    assert!(marker.exists(), "the real compiler must still run");
}

/// Probes exec the real compiler DIRECTLY: no permit, no `wrap_with` —
/// they must stay snappy under a full pool and must not depend on a
/// healthy sccache. All permits held + wrap fixture present: `-vV`
/// completes and the wrap marker stays absent.
#[test]
fn probe_bypasses_wrap_chain_and_permits() {
    let rig = Rig::new();
    let real_marker = rig.root.join("real");
    let wrap_marker = rig.root.join("wrap");
    let real = rig.script(
        "rustc.sh",
        &format!("echo real >> {}", real_marker.display()),
    );
    let wrap = rig.script(
        "wrap.sh",
        &format!("echo wrap >> {}\nexec \"$@\"", wrap_marker.display()),
    );
    let config = rig.config_with_wrap("gov.toml", 1, 0, false, true, &wrap);
    let _held = hold_exclusive(&rig.permit("permit-local-0"));

    let mut probe = spawn_governor(&config, &real, &["-vV"], &[]);
    assert!(wait_deadline(&mut probe, GENEROUS).unwrap().success());
    assert!(real_marker.exists(), "probe must exec the real compiler");
    assert!(
        !wrap_marker.exists(),
        "probe must bypass the wrap_with chain (it ran through sccache)"
    );
    assert_eq!(rig.log(), "", "probes must not log");
}

// -------------------------------------------------------- fail open ----

/// Fail-open: no config at the configured path — the governor skips
/// permits, and with no config there is no `wrap_with` either: it execs
/// the compiler cargo handed it as argv[1], directly.
#[test]
fn missing_config_fails_open_execs_argv1() {
    let rig = Rig::new();
    let marker = rig.root.join("marker");
    let real = rig.script("rustc.sh", &format!("echo fake >> {}", marker.display()));
    let missing = rig.root.join("no-such-config.toml");
    let mut child = spawn_governor(&missing, &real, &["--crate-name", "x"], &[]);
    assert!(wait_deadline(&mut child, GENEROUS).unwrap().success());
    assert!(
        marker.exists(),
        "fail-open must still run the real compiler"
    );
}

/// Fail-open: an unparseable config behaves exactly like a missing one.
#[test]
fn unparseable_config_fails_open() {
    let rig = Rig::new();
    let marker = rig.root.join("marker");
    let real = rig.script("rustc.sh", &format!("echo fake >> {}", marker.display()));
    let config = rig.root.join("broken.toml");
    std::fs::write(&config, "enabled = maybe\n").unwrap();
    let mut child = spawn_governor(&config, &real, &[], &[]);
    assert!(wait_deadline(&mut child, GENEROUS).unwrap().success());
    assert!(marker.exists());
}

/// Fail-open: the secondary per-invocation env override.
#[test]
fn env_off_bypasses_permits() {
    let rig = Rig::new();
    let marker = rig.root.join("marker");
    let script = rig.script("done.sh", &format!("echo done >> {}", marker.display()));
    let config = rig.config("gov.toml", 1, 0, false, true);
    let _held = hold_exclusive(&rig.permit("permit-local-0"));
    let mut child = spawn_governor(&config, &script, &[], &[("INTENDANT_GOVERNOR", "off")]);
    assert!(wait_deadline(&mut child, GENEROUS).unwrap().success());
    assert!(marker.exists());
}

/// Fail-open: a config with zero permits governs nothing.
#[test]
fn zero_permits_fails_open() {
    let rig = Rig::new();
    let marker = rig.root.join("marker");
    let script = rig.script("done.sh", &format!("echo done >> {}", marker.display()));
    let config = rig.config("gov.toml", 0, 0, false, true);
    let mut child = spawn_governor(&config, &script, &[], &[]);
    assert!(wait_deadline(&mut child, GENEROUS).unwrap().success());
    assert!(marker.exists());
}

// ---------------------------------------------------- wiring guards ----

/// Misconfiguration guard: cargo handing the governor ITSELF as the
/// compiler (a legacy `[build] rustc = …rustc-governor` line left next to
/// the new `rustc-wrapper` wiring) must refuse (exit 127) instead of
/// exec-looping into a fork bomb.
#[test]
fn refuses_to_exec_itself() {
    let rig = Rig::new();
    let config = rig.config("gov.toml", 1, 0, false, true);
    let mut child = Command::new(GOVERNOR)
        .arg(GOVERNOR)
        .args(["--crate-name", "x"])
        .env("INTENDANT_GOVERNOR_CONFIG", &config)
        .env_remove("INTENDANT_GOVERNOR")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let status = wait_deadline(&mut child, GENEROUS).unwrap();
    assert_eq!(status.code(), Some(127));
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.contains("refusing to exec itself"), "{stderr:?}");
}

/// No argv[1] at all is not a build (there is nothing to run): loud usage
/// error, so a mis-wired account is caught immediately instead of
/// half-working.
#[test]
fn missing_compiler_argv_is_a_loud_error() {
    let rig = Rig::new();
    let config = rig.config("gov.toml", 1, 0, false, true);
    let mut child = Command::new(GOVERNOR)
        .env("INTENDANT_GOVERNOR_CONFIG", &config)
        .env_remove("INTENDANT_GOVERNOR")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let status = wait_deadline(&mut child, GENEROUS).unwrap();
    assert_eq!(status.code(), Some(127));
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.contains("argv[1]"), "{stderr:?}");
}
