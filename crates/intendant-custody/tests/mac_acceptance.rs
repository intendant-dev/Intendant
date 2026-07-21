//! The Track K acceptance rig (ruling Q7, binding): prove on a real
//! macOS keychain that custody discriminates callers — the binary that
//! created the wrapping-key item retrieves silently, and a *different*
//! binary (the spawned `custody-probe` helper, an unregistered code
//! identity replaying the incident shape) fails closed with the
//! non-interactive deny class instead of reading the key or hanging on
//! a prompt.
//!
//! Hermetic by doctrine: a throwaway file-backed keychain inside a temp
//! dir — never the login keychain — and keychain UI disabled for the
//! whole process, so both legs are deterministic on dev Macs and on the
//! headless CI listener alike. The interactive prompt lane (a human
//! clicking "Always Allow") is deliberately NOT here; it lives in
//! `tests/skills/` and runs on operator hardware only.
//!
//! An integration test (not an inline module) because the rig's whole
//! point is spawning a second, differently-identified binary — cargo
//! only exposes `CARGO_BIN_EXE_*` to integration tests.

#![cfg(target_os = "macos")]

use std::process::Command;

use intendant_custody::mac_keychain::{disable_keychain_ui, ScopedKeychainKeyProvider};
use intendant_custody::{probe_exit, BackendKind, CustodyBackend, WrappedBlobBackend};

const KEYCHAIN_PASSWORD: &str = "intendant-acceptance-rig";
const ENTRY: &str = "access-certs/client.key";
const MATERIAL: &[u8] = b"RIG PRIVATE KEY MATERIAL (not a real key)";

#[test]
fn keychain_custody_discriminates_callers_without_ui() {
    // Process-global UI disable: a leg that would prompt must error, not
    // hang. Held for both legs — the silent leg proves it never needed
    // interaction in the first place.
    let _ui_lock = disable_keychain_ui().expect("disable keychain UI for the rig");

    let tmp = tempfile::tempdir().expect("rig temp dir");
    let keychain_path = tmp.path().join("acceptance.keychain");
    let blob_dir = tmp.path().join("blobs");

    let provider = ScopedKeychainKeyProvider::create(&keychain_path, KEYCHAIN_PASSWORD)
        .expect("create throwaway rig keychain");
    let backend = WrappedBlobBackend::new(
        &blob_dir,
        BackendKind::MacKeychainWrapped,
        Box::new(provider),
    )
    .expect("assemble rig backend");

    // ── Leg 1: the creating caller stores and retrieves silently. ──
    // The store mints the wrapping key into the rig keychain; the item's
    // ACL records this test binary as its trusted application.
    backend
        .store(ENTRY, MATERIAL)
        .expect("creating caller must store without interaction");
    let retrieved = backend
        .retrieve(ENTRY)
        .expect("creating caller must retrieve without interaction");
    assert_eq!(retrieved.as_bytes(), MATERIAL);

    // ── Leg 2: an unregistered binary fails closed, non-interactively. ──
    // `custody-probe` is a different executable, so the item ACL does not
    // trust it; with UI disabled the ACL confirmation must surface as the
    // named deny class — never the material, never a hang.
    let denied = probe(&keychain_path, &blob_dir, ENTRY);
    assert_eq!(
        denied.status.code(),
        Some(probe_exit::DENIED_NON_INTERACTIVE),
        "unregistered caller must land on DeniedNonInteractive; probe stderr:\n{}",
        String::from_utf8_lossy(&denied.stderr)
    );
    let denied_stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(
        denied_stderr.contains("denied non-interactively"),
        "probe stderr must carry the named deny class, got:\n{denied_stderr}"
    );

    // ── Leg 3: absent entries answer NotFound without touching the
    // keystore, even for the unregistered caller (no spurious deny/prompt
    // surface for entries that do not exist).
    let absent = probe(&keychain_path, &blob_dir, "access-certs/absent.key");
    assert_eq!(
        absent.status.code(),
        Some(probe_exit::NOT_FOUND),
        "absent entry must answer NotFound before any keystore access; probe stderr:\n{}",
        String::from_utf8_lossy(&absent.stderr)
    );
}

fn probe(
    keychain_path: &std::path::Path,
    blob_dir: &std::path::Path,
    entry: &str,
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_custody-probe"))
        .arg(keychain_path)
        .arg(KEYCHAIN_PASSWORD)
        .arg(blob_dir)
        .arg(entry)
        .output()
        .expect("spawn custody-probe")
}
