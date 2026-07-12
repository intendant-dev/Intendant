//! Thin flock(2)/fcntl(2) wrappers. The crate's `unsafe` lives here and in
//! `permits::current_username`, each block wrapping exactly one libc call
//! (repo convention: minimal, `// SAFETY:`-commented islands).
//!
//! flock, not fcntl locks, on purpose: flock locks belong to the open file
//! description, survive exec(2) (once FD_CLOEXEC is cleared), are inherited
//! by nothing else we spawn (we spawn nothing), and evaporate when the
//! holder dies — which is the entire crash-release story. Mode is
//! irrelevant to flock, so read-only opens of the root-owned 0644 permit
//! files lock fine for every account.

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

/// Rust's std opens every file O_CLOEXEC; a permit's flock must survive the
/// exec into the governed chain — the sccache client, or the real rustc
/// when `wrap_with` is unset (the flock IS the permit). Load-bearing —
/// covered by the `permit_lock_survives_exec` acceptance test.
pub(crate) fn clear_cloexec(file: &File) -> io::Result<()> {
    // SAFETY: the fd is owned by `file` and stays open across both calls;
    // F_GETFD moves no memory.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above; F_SETFD only updates the fd's flag word.
    let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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

    #[test]
    fn clear_cloexec_clears_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let f = File::create(dir.path().join("fd")).unwrap();
        // SAFETY: `f` owns an open fd; F_GETFD moves no memory.
        let before = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFD) };
        assert!(
            before >= 0 && (before & libc::FD_CLOEXEC) != 0,
            "std opens O_CLOEXEC"
        );
        clear_cloexec(&f).unwrap();
        // SAFETY: as above.
        let after = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFD) };
        assert!(after >= 0 && (after & libc::FD_CLOEXEC) == 0);
    }
}
