//! The machine-wide compile-permit pool: flock(2) files with class
//! reservations and a demand gate.
//!
//! Layout inside `permit_dir` (names are minted by
//! `scripts/ci/install-governor-macos.sh` in production — non-root accounts
//! cannot create files in the root-owned dir — and created on the fly in
//! user-writable dirs such as test rigs; interlock: change the naming here
//! and in the installer together):
//!
//! - `permit-local-<i>`, i < local_reserved  — the interactive reservation
//! - `permit-ci-<i>`,    i < ci_reserved     — the CI reservation
//! - `demand-local`, `demand-ci`             — one demand file per class
//! - `link-<i>`,         i < link_slots      — the heavyweight-link slots
//!   (machine-global, classless: host memory doesn't care whose link it
//!   is, so there is no reservation split and no demand gate)
//!
//! Protocol:
//! - Holding a permit = holding LOCK_EX on its file. The governor holds
//!   the lock itself for the governed chain's whole run — it stays alive
//!   as the chain's parent, and the fd keeps std's O_CLOEXEC so no child
//!   ever inherits it — and the kernel releases it when the governor
//!   exits, however it exits: crash release is structural.
//! - A waiter holds LOCK_SH on its OWN class's demand file for the whole
//!   wait, and releases it the moment it holds a permit.
//! - Borrowing: before touching a foreign-class permit, probe that class's
//!   demand file with LOCK_EX|LOCK_NB. Success (released immediately) means
//!   no waiters ⇒ the class's spare capacity may be borrowed; failure means
//!   the class has waiters ⇒ its reservation is honored. A failed probe can
//!   also mean another borrower's probe won the same instant — a transient,
//!   conservative false "has waiters" (we skip borrowing for one poll tick).
//! - Waiting is a 100ms LOCK_EX|LOCK_NB poll over every eligible permit —
//!   never a blocking flock on a permit: blocking flock has no timeout, and
//!   parking inside the kernel on a foreign permit would bypass the demand
//!   gate for the rest of the wait.
//! - Nothing is ever killed or signalled; borrowed permits return naturally
//!   when their holder exits.
//!
//! Link-slot ordering invariant (load-bearing — see `acquire_link_slot`):
//! a heavyweight invocation takes its LINK SLOT FIRST, and only then its
//! ordinary permit. **No ordinary-permit hoarding**: an invocation queued
//! on the link gate holds zero ordinary permits, so however many
//! heavyweights pile up, ordinary compiles keep the rest of the pool
//! (three simultaneous final links — the observed cargo behavior — pin
//! one slot and zero permits, not three permits). **Deadlock-free**: an
//! ordinary-permit holder never waits on the link slot (only heavyweights
//! do, and they acquired it first), so the wait graph has no cycle. The
//! cost — a held slot idling while its owner queues for an ordinary
//! permit — delays other *links* only, which is the acceptable direction.
//! No FIFO/fairness guarantee exists in the flock+poll design; soak
//! telemetry (govlog's wait fields) decides whether one is ever needed.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::config::{self, Config};
use crate::flock;

pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    Local,
    Ci,
}

impl Class {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Class::Local => "local",
            Class::Ci => "ci",
        }
    }

    fn other(self) -> Class {
        match self {
            Class::Local => Class::Ci,
            Class::Ci => Class::Local,
        }
    }
}

/// Everything not listed in `ci_users` — including an unresolvable
/// username — is local (the design: CI is the explicit allowlist).
pub(crate) fn classify(username: Option<&str>, cfg: &Config) -> Class {
    match username {
        Some(user) if cfg.ci_users.iter().any(|ci| ci == user) => Class::Ci,
        _ => Class::Local,
    }
}

/// Username of the effective uid via getpwuid_r — deliberately not
/// `$USER`/`$LOGNAME`, which go stale under sudo/setuid and are trivially
/// spoofed into the wrong class.
pub(crate) fn current_username() -> Option<String> {
    // SAFETY: geteuid(2) has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    let mut buf = vec![0_u8; 256];
    loop {
        // SAFETY: `passwd` is a plain C struct for which all-zero bytes are
        // a valid (if meaningless) value; getpwuid_r fully initializes it
        // on success.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer references a live local; `buf` outlives the
        // call and its true length is passed alongside it.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                &mut pwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE {
            if buf.len() >= 64 * 1024 {
                return None;
            }
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        // SAFETY: on success `result` points at `pwd`, whose pw_name is a
        // NUL-terminated string inside `buf`; both are still live.
        let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
        return Some(name.to_string_lossy().into_owned());
    }
}

pub(crate) struct AcquiredPermit {
    /// Keeping this `File` open IS the permit — held for RAII alone,
    /// never read: the governor holds it, parent-side, for the governed
    /// chain's whole run — the fd keeps std's O_CLOEXEC so no child (the
    /// sccache client, or any server it daemonizes) can inherit it — and
    /// the kernel releases the flock when the `File` closes (drop, or
    /// any governor exit).
    pub(crate) _file: File,
    pub(crate) name: String,
    pub(crate) wait_ms: u64,
}

/// A held heavyweight-link slot. Exactly the `AcquiredPermit` story:
/// keeping the `File` open IS the slot, parent-held with O_CLOEXEC intact,
/// kernel-released on any exit.
pub(crate) struct AcquiredLinkSlot {
    pub(crate) _file: File,
    pub(crate) name: String,
    pub(crate) wait_ms: u64,
}

/// Outcome of gating a heavyweight link.
pub(crate) enum LinkGate {
    /// Slot held: the link is serialized.
    Held(AcquiredLinkSlot),
    /// `link_slots = 0`: the gate is configured off on this box.
    Off,
    /// No slot file was usable (config grown past the installer-minted
    /// files in a root-owned dir, or the dir denies creation). Link
    /// gating degrades — the caller logs it and proceeds — but ordinary
    /// governance NEVER rides with it: only the global kill-switch paths
    /// drop the whole governor.
    Degraded,
    /// The config vanished or disabled mid-wait: the whole invocation
    /// fails open (the caller drops nothing — no slot was acquired).
    FailOpen,
}

fn permit_name(class: Class, i: u32) -> String {
    format!("permit-{}-{i}", class.as_str())
}

fn demand_name(class: Class) -> String {
    format!("demand-{}", class.as_str())
}

fn link_slot_name(i: u32) -> String {
    format!("link-{i}")
}

/// Open (or, where the directory allows it, create) a lock file. flock(2)
/// only needs an open fd — read-only is enough against the root-owned 0644
/// files of a production install; the create fallback serves tempdir rigs
/// and any user-writable permit dir. Concurrent creators share the inode
/// (O_CREAT without O_EXCL), so their locks contend correctly.
fn open_lock_file(path: &Path) -> Option<File> {
    match OpenOptions::new().read(true).open(path) {
        Ok(f) => Some(f),
        Err(e) if e.kind() == io::ErrorKind::NotFound => OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .ok(),
        Err(_) => None,
    }
}

/// Open whichever of the class's permit files are usable; the ones that
/// aren't (e.g. counts grown in config without the installer re-run that
/// mints the new root-owned files) are simply not part of the pool.
fn open_permits(dir: &Path, class: Class, count: u32) -> Vec<(String, File)> {
    (0..count)
        .filter_map(|i| {
            let name = permit_name(class, i);
            open_lock_file(&dir.join(&name)).map(|f| (name, f))
        })
        .collect()
}

/// First lockable permit, moved out of the pool.
fn try_take(pool: &mut Vec<(String, File)>) -> Option<(String, File)> {
    let idx = pool
        .iter()
        .position(|(_, f)| flock::try_lock_exclusive(f))?;
    Some(pool.swap_remove(idx))
}

/// Probe the foreign class's demand gate. Success ⇒ no registered waiters
/// ⇒ borrowing its spare permits is allowed right now.
fn foreign_has_no_waiters(demand: &File) -> bool {
    if flock::try_lock_exclusive(demand) {
        flock::unlock(demand);
        true
    } else {
        false
    }
}

/// Acquire one machine-global link slot for a heavyweight link, waiting as
/// long as it takes. Called BEFORE `acquire` — the ordering invariant in
/// the module doc: a waiter here holds nothing, so it can hoard no
/// ordinary capacity and can complete no deadlock cycle. No demand gate:
/// the slots are classless, and the only takers are heavyweights in this
/// same single queue. The live kill switch is honored mid-wait exactly
/// like the permit poll.
pub(crate) fn acquire_link_slot(cfg: &Config, config_path: &Path) -> LinkGate {
    if cfg.link_slots == 0 {
        return LinkGate::Off;
    }
    let dir = &cfg.permit_dir;
    let _ = fs::create_dir_all(dir);
    let mut slots: Vec<(String, File)> = (0..cfg.link_slots)
        .filter_map(|i| {
            let name = link_slot_name(i);
            open_lock_file(&dir.join(&name)).map(|f| (name, f))
        })
        .collect();
    if slots.is_empty() {
        return LinkGate::Degraded;
    }
    let start = Instant::now();
    loop {
        if let Some((name, file)) = try_take(&mut slots) {
            return LinkGate::Held(AcquiredLinkSlot {
                _file: file,
                name,
                wait_ms: start.elapsed().as_millis() as u64,
            });
        }
        // Same live kill switch as the permit poll below: a flipped
        // `enabled = false` or a deleted config unwedges link waiters
        // within one poll tick, fail-open.
        match config::load(config_path) {
            Some(live) if live.enabled => {}
            _ => return LinkGate::FailOpen,
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Acquire one machine-wide compile permit for `class`, waiting as long as
/// it takes. `None` always means FAIL OPEN (run ungoverned): zero configured
/// permits, an unusable permit dir, an unregisterable wait, or the kill
/// switch flipping mid-wait — never "denied".
pub(crate) fn acquire(cfg: &Config, class: Class, config_path: &Path) -> Option<AcquiredPermit> {
    if cfg.local_reserved == 0 && cfg.ci_reserved == 0 {
        return None;
    }
    let dir = &cfg.permit_dir;
    // Production dirs come from the installer; tempdir rigs (and any
    // user-writable permit_dir) are created on the fly. Failure is fine —
    // open_lock_file below decides whether anything is actually usable.
    let _ = fs::create_dir_all(dir);

    let (own_count, foreign_count) = match class {
        Class::Local => (cfg.local_reserved, cfg.ci_reserved),
        Class::Ci => (cfg.ci_reserved, cfg.local_reserved),
    };
    let mut own = open_permits(dir, class, own_count);
    let mut foreign = open_permits(dir, class.other(), foreign_count);
    if own.is_empty() && foreign.is_empty() {
        // Nothing lockable at all: fail open.
        return None;
    }

    let start = Instant::now();
    let acquired = |start: Instant, (name, file): (String, File)| {
        Some(AcquiredPermit {
            _file: file,
            name,
            wait_ms: start.elapsed().as_millis() as u64,
        })
    };

    // Own-class reservation first.
    if let Some(p) = try_take(&mut own) {
        return acquired(start, p);
    }

    // Borrow the other class's spare capacity — but only through its demand
    // gate. An unopenable foreign demand file means the gate cannot be
    // probed: never borrow blind.
    let foreign_demand = open_lock_file(&dir.join(demand_name(class.other())));
    if !foreign.is_empty() {
        if let Some(gate) = &foreign_demand {
            if foreign_has_no_waiters(gate) {
                if let Some(p) = try_take(&mut foreign) {
                    return acquired(start, p);
                }
            }
        }
    }

    // Nothing free: register demand — LOCK_SH held for the entire wait so
    // foreign borrowers keep their hands off this class's reservation —
    // then poll. If demand can't be registered, fail open rather than wait
    // unregistered: borrowers couldn't see us, and we could starve behind
    // them indefinitely.
    let own_demand = open_lock_file(&dir.join(demand_name(class)))?;
    if !flock::lock_shared_blocking(&own_demand) {
        return None;
    }
    loop {
        if let Some(p) = try_take(&mut own) {
            flock::unlock(&own_demand);
            return acquired(start, p);
        }
        if !foreign.is_empty() {
            if let Some(gate) = &foreign_demand {
                if foreign_has_no_waiters(gate) {
                    if let Some(p) = try_take(&mut foreign) {
                        flock::unlock(&own_demand);
                        return acquired(start, p);
                    }
                }
            }
        }
        // Live kill switch for in-flight waiters too, not just new
        // invocations: a flipped `enabled = false` (or a deleted config)
        // unwedges every governed process within one poll tick, fail-open.
        // Only `enabled` is honored mid-wait; counts/paths stay as loaded.
        match config::load(config_path) {
            Some(live) if live.enabled => {}
            _ => {
                flock::unlock(&own_demand);
                return None;
            }
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rig(local: u32, ci: u32) -> (tempfile::TempDir, Config, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let permit_dir = tmp.path().join("permits");
        let cfg = Config {
            enabled: true,
            permit_dir: permit_dir.clone(),
            local_reserved: local,
            ci_reserved: ci,
            link_slots: 1,
            ci_users: vec!["_intendant-ci".into(), "ci".into()],
            wrap_with: None,
        };
        let config_path = tmp.path().join("governor.toml");
        std::fs::write(
            &config_path,
            format!(
                "enabled = true\npermit_dir = \"{}\"\nlocal_reserved = {local}\nci_reserved = {ci}\n",
                permit_dir.display()
            ),
        )
        .unwrap();
        (tmp, cfg, config_path)
    }

    #[test]
    fn classify_matches_ci_users_and_defaults_local() {
        let cfg = Config::default();
        assert_eq!(classify(Some("_intendant-ci"), &cfg), Class::Ci);
        assert_eq!(classify(Some("ci"), &cfg), Class::Ci);
        assert_eq!(classify(Some("somebody"), &cfg), Class::Local);
        assert_eq!(classify(None, &cfg), Class::Local);
    }

    #[test]
    fn current_username_resolves() {
        let name = current_username().expect("euid must resolve to a user");
        assert!(!name.is_empty());
    }

    #[test]
    fn acquire_prefers_own_class_then_borrows_idle_foreign() {
        let (_tmp, cfg, path) = rig(1, 1);
        let a = acquire(&cfg, Class::Local, &path).unwrap();
        assert_eq!(a.name, "permit-local-0");
        // Own class exhausted, no CI demand registered: borrow.
        let b = acquire(&cfg, Class::Local, &path).unwrap();
        assert_eq!(b.name, "permit-ci-0");
        drop(a);
        // Own reservation is free again and preferred.
        let c = acquire(&cfg, Class::Local, &path).unwrap();
        assert_eq!(c.name, "permit-local-0");
    }

    #[test]
    fn zero_permits_fails_open() {
        let (_tmp, cfg, path) = rig(0, 0);
        assert!(acquire(&cfg, Class::Local, &path).is_none());
    }

    #[test]
    fn link_gate_off_and_degraded_never_block() {
        let (_tmp, mut cfg, path) = rig(1, 0);
        cfg.link_slots = 0;
        assert!(matches!(acquire_link_slot(&cfg, &path), LinkGate::Off));
        // An unusable permit dir (here: a plain file where the dir should
        // be) means no slot file can be opened or created: Degraded, so
        // the caller keeps ordinary governance instead of failing open.
        cfg.link_slots = 1;
        cfg.permit_dir = path.clone();
        assert!(matches!(acquire_link_slot(&cfg, &path), LinkGate::Degraded));
    }

    #[test]
    fn link_slots_serialize_and_release() {
        let (_tmp, cfg, path) = rig(2, 0);
        let a = match acquire_link_slot(&cfg, &path) {
            LinkGate::Held(slot) => slot,
            _ => panic!("first heavyweight must take the slot"),
        };
        assert_eq!(a.name, "link-0");
        // The single slot is held: a second taker polls. Prove it waits
        // by watching a thread not finish, then release and join.
        let (cfg2, path2) = (cfg.clone(), path.clone());
        let waiter = std::thread::spawn(move || acquire_link_slot(&cfg2, &path2));
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !waiter.is_finished(),
            "second heavyweight must queue on the held link slot"
        );
        drop(a);
        match waiter.join().unwrap() {
            LinkGate::Held(slot) => assert_eq!(slot.name, "link-0"),
            _ => panic!("waiter must acquire after release"),
        }
    }

    #[test]
    fn multiple_link_slots_admit_that_many() {
        let (_tmp, mut cfg, path) = rig(2, 0);
        cfg.link_slots = 2;
        let a = acquire_link_slot(&cfg, &path);
        let b = acquire_link_slot(&cfg, &path);
        let (a, b) = match (a, b) {
            (LinkGate::Held(a), LinkGate::Held(b)) => (a, b),
            _ => panic!("two slots must admit two links"),
        };
        assert_ne!(a.name, b.name);
    }

    #[test]
    fn waiter_respects_foreign_demand_then_takes_own_release() {
        let (_tmp, cfg, path) = rig(1, 1);
        let held = acquire(&cfg, Class::Local, &path).unwrap();
        assert_eq!(held.name, "permit-local-0");
        // Register CI demand so Local may not borrow the idle CI permit.
        let demand_ci = open_lock_file(&cfg.permit_dir.join("demand-ci")).unwrap();
        assert!(flock::lock_shared_blocking(&demand_ci));

        let (cfg2, path2) = (cfg.clone(), path.clone());
        let waiter = std::thread::spawn(move || acquire(&cfg2, Class::Local, &path2));
        // Generous settle time; the waiter must still be polling (the CI
        // permit is free but demand-gated).
        std::thread::sleep(Duration::from_millis(400));
        assert!(
            !waiter.is_finished(),
            "local waiter must not borrow a contested CI permit"
        );

        drop(held);
        let got = waiter
            .join()
            .unwrap()
            .expect("waiter must acquire after release");
        assert_eq!(got.name, "permit-local-0");
        flock::unlock(&demand_ci);
    }
}
