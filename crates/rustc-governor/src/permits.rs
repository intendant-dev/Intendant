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
//!
//! Protocol:
//! - Holding a permit = holding LOCK_EX on its file. The lock rides the fd
//!   across exec(2) into the governed chain (the blocking sccache client,
//!   or the real rustc when `wrap_with` is unset) and is released by the
//!   kernel when that process exits, however it exits — crash release is
//!   structural.
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
    /// Keeping this `File` open (with FD_CLOEXEC cleared) across the exec
    /// IS the permit; the kernel releases the flock when the process exits.
    pub(crate) file: File,
    pub(crate) name: String,
    pub(crate) wait_ms: u64,
}

fn permit_name(class: Class, i: u32) -> String {
    format!("permit-{}-{i}", class.as_str())
}

fn demand_name(class: Class) -> String {
    format!("demand-{}", class.as_str())
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
            file,
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
