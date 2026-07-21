//! Acceptance-rig helper: the UNREGISTERED caller (Track K ruling Q7).
//!
//! The acceptance test creates a throwaway keychain and stores a wrapping
//! key from its own process; this binary is the *other* program — a
//! different code identity the keychain item's ACL does not trust. It
//! disables keychain UI (so an ACL confirmation becomes a deterministic
//! error instead of a prompt), unlocks the keychain with its explicit
//! password (removing the lock state from the equation), and attempts a
//! retrieval. Its exit code reports the custody outcome via
//! [`intendant_custody::probe_exit`].
//!
//! Never point this at the login keychain; it exists for rig keychains in
//! temp dirs.

fn main() {
    std::process::exit(run());
}

#[cfg(target_os = "macos")]
fn run() -> i32 {
    use intendant_custody::mac_keychain::{disable_keychain_ui, ScopedKeychainKeyProvider};
    use intendant_custody::{probe_exit, BackendKind, CustodyBackend, CustodyError};

    let args: Vec<String> = std::env::args().skip(1).collect();
    let [keychain_path, keychain_password, blob_dir, entry] = args.as_slice() else {
        eprintln!("usage: custody-probe <keychain-path> <keychain-password> <blob-dir> <entry>");
        return probe_exit::USAGE;
    };

    let _ui_lock = match disable_keychain_ui() {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("custody-probe: cannot disable keychain UI: {error}");
            return probe_exit::USAGE;
        }
    };

    let provider = ScopedKeychainKeyProvider::open(keychain_path, keychain_password);
    let backend = match intendant_custody::WrappedBlobBackend::new(
        blob_dir,
        BackendKind::MacKeychainWrapped,
        Box::new(provider),
    ) {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("custody-probe: backend setup failed: {error}");
            return probe_exit::USAGE;
        }
    };

    match backend.retrieve(entry) {
        Ok(secret) => {
            // Outcome only — material never reaches stdout/stderr.
            eprintln!(
                "custody-probe: RETRIEVED {} bytes for {entry}",
                secret.as_bytes().len()
            );
            probe_exit::RETRIEVED
        }
        Err(error) => {
            eprintln!("custody-probe: {error}");
            match error {
                CustodyError::DeniedNonInteractive { .. } => probe_exit::DENIED_NON_INTERACTIVE,
                CustodyError::NotFound { .. } => probe_exit::NOT_FOUND,
                CustodyError::Unsealable { .. } => probe_exit::UNSEALABLE,
                CustodyError::BackendUnavailable { .. } => probe_exit::BACKEND_UNAVAILABLE,
                CustodyError::InvalidName { .. } | CustodyError::Io { .. } => probe_exit::OTHER,
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn run() -> i32 {
    eprintln!("custody-probe: the keychain acceptance rig is macOS-only");
    intendant_custody::probe_exit::USAGE
}
