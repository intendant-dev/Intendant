//! Thin flock(2) wrappers. The crate's `unsafe` lives here, in
//! `permits::current_username`, and in `main.rs`'s signal
//! forwarding/re-raise — minimal, `// SAFETY:`-commented islands (repo
//! convention).
//!
//! flock, not fcntl locks, on purpose: flock locks belong to the open
//! file description and evaporate when the holder dies — the entire
//! crash-release story. The permit-holding fd is never passed to a
//! child: it keeps std's O_CLOEXEC, and the governor holds it as the
//! spawned chain's PARENT (an inherited fd in a long-lived child — the
//! sccache server the client daemonizes on demand — kept permits locked
//! for hours in production, 2026-07-12). Mode is irrelevant to flock, so
//! read-only opens of the root-owned 0644 permit files lock fine for
//! every account.

use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;

fn flock_op(file: &File, op: libc::c_int) -> io::Result<()> {
    loop {
        // SAFETY: `file` owns an fd that stays open for the duration of the
        // call; flock(2) takes only that fd plus an operation flag and
        // touches no caller memory.
        let rc = unsafe { libc::flock(file.as_raw_fd(), op) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return Err(err);
        }
    }
}

/// LOCK_EX|LOCK_NB: true iff the exclusive lock was taken.
pub(crate) fn try_lock_exclusive(file: &File) -> bool {
    flock_op(file, libc::LOCK_EX | libc::LOCK_NB).is_ok()
}

/// Blocking LOCK_SH, used only to register demand. Bounded in practice: the
/// only LOCK_EX takers on demand files are gate probes, which release
/// immediately (permits — where a holder keeps LOCK_EX for a whole compile
/// — are never locked blocking; the poll loop owns that waiting).
pub(crate) fn lock_shared_blocking(file: &File) -> bool {
    flock_op(file, libc::LOCK_SH).is_ok()
}

pub(crate) fn unlock(file: &File) {
    let _ = flock_op(file, libc::LOCK_UN);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_locks_exclude_across_open_descriptions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("permit");
        let a = File::create(&path).unwrap();
        let b = File::open(&path).unwrap();
        assert!(try_lock_exclusive(&a));
        // Same process, distinct open file description: still excluded.
        assert!(!try_lock_exclusive(&b));
        unlock(&a);
        assert!(try_lock_exclusive(&b));
    }

    #[test]
    fn shared_lock_blocks_exclusive_probe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demand");
        let holder = File::create(&path).unwrap();
        let prober = File::open(&path).unwrap();
        assert!(lock_shared_blocking(&holder));
        assert!(!try_lock_exclusive(&prober));
        unlock(&holder);
        assert!(try_lock_exclusive(&prober));
    }

    /// The permit fd must stay invisible to children: std's O_CLOEXEC is
    /// load-bearing for the parent-held permit design (nothing in this
    /// crate may clear it — a cleared fd inherited by the daemonized
    /// sccache server is exactly the 2026-07-12 permit leak).
    #[test]
    fn std_opens_files_cloexec() {
        let dir = tempfile::tempdir().unwrap();
        let f = File::create(dir.path().join("fd")).unwrap();
        // SAFETY: `f` owns an open fd; F_GETFD moves no memory.
        let flags = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFD) };
        assert!(
            flags >= 0 && (flags & libc::FD_CLOEXEC) != 0,
            "std must open files O_CLOEXEC"
        );
    }
}
