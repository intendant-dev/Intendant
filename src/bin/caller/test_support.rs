//! Shared infrastructure for the caller's inline `#[cfg(test)]` modules.

/// Serializes tests that read or mutate process-global environment
/// variables (`HOME`/`USERPROFILE`, provider API keys, and friends). Env
/// vars are process-wide and `cargo test` runs tests concurrently in one
/// binary, so an unserialized `set_var`/`remove_var` in one test races an
/// assert in another — a loaded-box flake that surfaces on CI runners long
/// before it reproduces on an idle dev machine. One crate-wide lock (not
/// one per module): tests in different modules share the same process
/// environment, so per-module locks still race each other. (Production
/// code must not smuggle state through the process env at all — the
/// user-display grant used to, and was moved to the autonomy guard with a
/// spawn-boundary env derivation for runtime children.)
///
/// Async tests take `.lock().await` (the guard is held across awaits, which
/// a `std` mutex would not allow); sync tests outside a runtime take
/// `.blocking_lock()`. Tokio's mutex does not poison, so a panicking test
/// cannot wedge the rest of the suite.
pub(crate) static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A fresh scratch directory owned by the calling thread and removed when
/// that thread exits — for libtest, when the test finishes (after a
/// `#[tokio::test]` body has already dropped its runtime and the tasks
/// that used the directory).
///
/// For the per-instance roots test helpers mint below a transport edge
/// (a test gateway's access store, a test supervisor's logs home, a
/// dashboard-control state root). Those used to be `temp_dir()` joins
/// that nothing removed, or `TempDir`s parked in a process-lifetime
/// static, which is never dropped: every `cargo test` run left ~150 dirs
/// behind, and the Linux CI runner's tmpfs had accumulated tens of
/// thousands (over 40% of its inodes) by 2026-10. A thread-local owner
/// gives each root its test's lifetime without threading a guard through
/// the many helpers that build these configs. Call it on the test's own
/// thread (or a runtime worker that lives as long as the test), never from
/// a short-lived helper thread, whose exit would remove the dir early.
pub(crate) fn thread_scratch_dir(prefix: &str) -> std::path::PathBuf {
    thread_local! {
        static SCRATCH: std::cell::RefCell<Vec<tempfile::TempDir>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    let dir = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .expect("create test scratch dir");
    let path = dir.path().to_path_buf();
    SCRATCH.with(|scratch| scratch.borrow_mut().push(dir));
    path
}
