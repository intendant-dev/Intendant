//! Serialization and durable writes for daemon-local authority state.
//!
//! `iam.json`, peer identity records, and the org revocation list live in
//! separate files under one access-certificate directory, but together they
//! form one security boundary. Every read-modify-write operation on those
//! files takes this lock so a daemon and an `intendant` CLI process cannot
//! overwrite each other's decisions with stale snapshots.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
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

fn process_locks() -> &'static Mutex<HashSet<PathBuf>> {
    static LOCKS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashSet::new()))
}

struct ProcessStoreLock {
    path: PathBuf,
}

impl Drop for ProcessStoreLock {
    fn drop(&mut self) {
        let mut locks = process_locks()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.remove(&self.path);
    }
}

struct AuthorityStoreLock {
    file: File,
    _process: ProcessStoreLock,
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

fn acquire_process_lock(cert_dir: &Path, started: Instant) -> AccessResult<ProcessStoreLock> {
    loop {
        let mut locks = process_locks().lock().map_err(|_| {
            AccessError(
                "authority-store process locks were poisoned; refusing to mutate authority state"
                    .to_string(),
            )
        })?;
        if locks.insert(cert_dir.to_path_buf()) {
            return Ok(ProcessStoreLock {
                path: cert_dir.to_path_buf(),
            });
        }
        drop(locks);
        if timed_out(started) {
            return Err(lock_timeout_error(&cert_dir.join(LOCK_FILE)));
        }
        std::thread::sleep(LOCK_RETRY);
    }
}

fn acquire_lock(cert_dir: &Path, started: Instant) -> AccessResult<AuthorityStoreLock> {
    let process = acquire_process_lock(cert_dir, started)?;

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
    intendant_core::state_paths::create_private_dir_all(cert_dir)?;
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
    intendant_core::state_paths::create_private_dir_all(parent)?;
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

/// Remove one authority-state file and durably record the directory change.
/// The caller must already hold [`with_lock`]. A missing file is already in
/// the requested state.
pub fn remove_file_locked(path: &Path) -> AccessResult<()> {
    let parent = path.parent().ok_or_else(|| {
        AccessError(format!(
            "authority-state path has no parent: {}",
            path.display()
        ))
    })?;
    match std::fs::remove_file(path) {
        Ok(()) => sync_parent(parent),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AccessError(format!("remove {}: {error}", path.display()))),
    }
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

    #[test]
    fn independent_stores_do_not_share_a_process_lock_slot() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let second_path = second.path().to_path_buf();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut worker = None;

        with_lock(first.path(), || {
            worker = Some(std::thread::spawn(move || {
                done_tx.send(with_lock(&second_path, || Ok(()))).unwrap();
            }));
            done_rx
                .recv_timeout(Duration::from_secs(1))
                .map_err(|error| {
                    AccessError(format!("independent store stayed blocked: {error}"))
                })??;
            Ok(())
        })
        .unwrap();
        worker.unwrap().join().unwrap();
    }
}
