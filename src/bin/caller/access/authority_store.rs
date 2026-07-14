//! Serialization and durable writes for daemon-local authority state.
//!
//! `iam.json`, peer identity records, and the org revocation list live in
//! separate files under one access-certificate directory, but together they
//! form one security boundary. Every read-modify-write operation on those
//! files takes this lock so a daemon and an `intendant` CLI process cannot
//! overwrite each other's decisions with stale snapshots.

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, TryLockError};
use std::time::{Duration, Instant};

use super::{AccessError, AccessResult};

const LOCK_FILE: &str = ".authority-store.lock";
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const LOCK_RETRY: Duration = Duration::from_millis(10);

thread_local! {
    /// Synchronous authority mutations can nest (an IAM transaction may
    /// revoke a peer record). The outer call owns both locks; same-directory
    /// nested calls reuse it, while cross-directory nesting fails closed.
    static HELD_STORE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

fn process_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct AuthorityStoreLock {
    file: File,
    _process: MutexGuard<'static, ()>,
}

impl Drop for AuthorityStoreLock {
    fn drop(&mut self) {
        let _ = File::unlock(&self.file);
    }
}

struct HeldStoreReset;

impl Drop for HeldStoreReset {
    fn drop(&mut self) {
        HELD_STORE.with(|held| *held.borrow_mut() = None);
    }
}

fn timed_out(started: Instant) -> bool {
    started.elapsed() >= LOCK_TIMEOUT
}

fn lock_timeout_error(path: &Path) -> AccessError {
    AccessError(format!(
        "timed out after {}ms waiting for authority-store lock {}; no state was changed",
        LOCK_TIMEOUT.as_millis(),
        path.display()
    ))
}

fn acquire_lock(cert_dir: &Path, started: Instant) -> AccessResult<AuthorityStoreLock> {
    let process = loop {
        match process_lock().try_lock() {
            Ok(guard) => break guard,
            Err(TryLockError::Poisoned(_)) => {
                return Err(AccessError(
                    "authority-store process lock was poisoned; refusing to mutate authority state"
                        .to_string(),
                ));
            }
            Err(TryLockError::WouldBlock) if timed_out(started) => {
                return Err(lock_timeout_error(&cert_dir.join(LOCK_FILE)));
            }
            Err(TryLockError::WouldBlock) => std::thread::sleep(LOCK_RETRY),
        }
    };

    let lock_path = cert_dir.join(LOCK_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| AccessError(format!("open {}: {error}", lock_path.display())))?;
    set_private_perms(&lock_path)?;
    loop {
        match File::try_lock(&file) {
            Ok(()) => {
                return Ok(AuthorityStoreLock {
                    file,
                    _process: process,
                });
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                if timed_out(started) {
                    return Err(lock_timeout_error(&lock_path));
                }
                std::thread::sleep(LOCK_RETRY);
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(AccessError(format!(
                    "lock authority store {}: {error}",
                    lock_path.display()
                )));
            }
        }
    }
}

/// Run one authority-store operation under the process and cross-process
/// locks. Calls may nest only for the same canonical certificate directory.
pub fn with_lock<T>(
    cert_dir: &Path,
    operation: impl FnOnce() -> AccessResult<T>,
) -> AccessResult<T> {
    std::fs::create_dir_all(cert_dir)?;
    let canonical = std::fs::canonicalize(cert_dir)
        .map_err(|error| AccessError(format!("resolve {}: {error}", cert_dir.display())))?;
    if let Some(held) = HELD_STORE.with(|current| current.borrow().clone()) {
        if held == canonical {
            return operation();
        }
        return Err(AccessError(format!(
            "authority-store mutation for {} attempted while {} is locked; refusing cross-store nesting",
            canonical.display(),
            held.display()
        )));
    }

    let lock = acquire_lock(&canonical, Instant::now())?;
    HELD_STORE.with(|held| *held.borrow_mut() = Some(canonical));
    let _reset = HeldStoreReset;
    let _lock = lock;
    operation()
}

/// Replace one authority-state file atomically with a uniquely named,
/// fsync'd 0600 file. The caller must already hold [`with_lock`].
pub fn atomic_write_private_locked(path: &Path, contents: &[u8]) -> AccessResult<()> {
    let parent = path.parent().ok_or_else(|| {
        AccessError(format!(
            "authority-state path has no parent: {}",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".intendant-authority-")
        .tempfile_in(parent)
        .map_err(|error| {
            AccessError(format!("create temp file in {}: {error}", parent.display()))
        })?;
    temporary
        .write_all(contents)
        .map_err(|error| AccessError(format!("write temp file for {}: {error}", path.display())))?;
    set_private_perms(temporary.path())?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| AccessError(format!("sync temp file for {}: {error}", path.display())))?;
    let persisted = temporary.persist(path).map_err(|error| {
        AccessError(format!(
            "atomically replace {}: {}",
            path.display(),
            error.error
        ))
    })?;
    persisted
        .sync_all()
        .map_err(|error| AccessError(format!("sync {}: {error}", path.display())))?;
    sync_parent(parent)?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> AccessResult<()> {
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| AccessError(format!("sync directory {}: {error}", parent.display())))
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> AccessResult<()> {
    // Windows does not offer a portable directory fsync through std. The
    // uniquely named file is flushed before and after atomic replacement.
    Ok(())
}

fn set_private_perms(path: &Path) -> AccessResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(path, permissions)?;
    }
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_store_lock_is_reentrant_and_atomic_write_is_private() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("state.json");
        with_lock(directory.path(), || {
            with_lock(directory.path(), || {
                atomic_write_private_locked(&path, b"one\n")
            })
        })
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn cross_store_nested_lock_fails_closed() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let error = with_lock(first.path(), || with_lock(second.path(), || Ok(()))).unwrap_err();
        assert!(error.to_string().contains("refusing cross-store nesting"));
    }
}
