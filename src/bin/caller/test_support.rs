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
