//! Custody backends for Intendant's private key material (Track K).
//!
//! One fail-closed trait in front of per-platform secret storage: entries
//! are named secrets, every outcome is a typed named error, and retrieval
//! hands back zeroizing buffers. The crate is a leaf — it emits no audit
//! events and knows nothing about the daemon; callers translate outcomes
//! into the custody trail and choose the backend/fallback chain, labeling
//! it honestly.
//!
//! Interim trust label (binding doctrine from the Track K ruling): before
//! a Developer ID + hardened-runtime binary exists, OS-keystore custody is
//! *bar-raising, not lane-sealing* — it defeats the casual same-uid file
//! read, not a patient same-uid attacker. Nothing in this crate may claim
//! otherwise.

mod file_backend;
mod names;
mod seal;
mod wrapped;

#[cfg(target_os = "macos")]
pub mod mac_keychain;

pub use file_backend::PlainFileBackend;
pub use names::validate_entry_name;
pub use wrapped::{WrappedBlobBackend, WrappingKeyProvider};

use zeroize::Zeroizing;

/// Retrieved secret material. The buffer is zeroized on drop; callers
/// that need a long-lived copy own that copy's hygiene.
pub struct Secret(Zeroizing<Vec<u8>>);

impl Secret {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Material never reaches logs or error strings.
        write!(f, "Secret({} bytes)", self.0.len())
    }
}

/// Where an entry's material physically lives. Every backend names its
/// kind so callers can label custody status honestly ("keychain-wrapped"
/// vs "labeled file mode") instead of inferring it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// Sealed blobs on disk, wrapping key held by the macOS keychain.
    MacKeychainWrapped,
    /// Plain 0600 files — the honest floor for platforms/contexts with no
    /// usable keystore. Same-uid readable; callers MUST label it.
    PlainFile,
}

impl std::fmt::Display for BackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendKind::MacKeychainWrapped => write!(f, "macos-keychain-wrapped"),
            BackendKind::PlainFile => write!(f, "plain-file"),
        }
    }
}

/// Every custody outcome is one of these, by name — callers never guess
/// from a string. Fail-closed: any error means "no material was served".
#[derive(Debug, thiserror::Error)]
pub enum CustodyError {
    #[error("invalid custody entry name {name:?}: {reason}")]
    InvalidName { name: String, reason: &'static str },
    #[error("custody entry {name} not found")]
    NotFound { name: String },
    /// The backend cannot serve at all right now (keystore missing,
    /// wrapping key absent without create permission, platform service
    /// down). The named trigger for a caller's labeled fallback.
    #[error("custody backend {backend} unavailable: {reason}")]
    BackendUnavailable {
        backend: BackendKind,
        reason: String,
    },
    /// The backend exists but refused without user interaction — the
    /// headless deny class (`errSecInteractionNotAllowed` and kin).
    /// Deliberately distinct from [`CustodyError::BackendUnavailable`]:
    /// this is the acceptance-test assertion for caller discrimination.
    #[error("custody backend {backend} denied non-interactively: {reason}")]
    DeniedNonInteractive {
        backend: BackendKind,
        reason: String,
    },
    /// The sealed blob failed its integrity check — wrong wrapping key or
    /// a tampered/entry-swapped blob. Never distinguished further.
    #[error("sealed custody blob for {name} failed its integrity check")]
    Unsealable { name: String },
    #[error("custody io ({context}): {message}")]
    Io { context: String, message: String },
}

impl CustodyError {
    pub(crate) fn io(context: impl Into<String>, error: std::io::Error) -> Self {
        CustodyError::Io {
            context: context.into(),
            message: error.to_string(),
        }
    }
}

/// The one custody interface. Implementations are per-platform storage
/// strategies; policy (which backend, what fallback, what label, what
/// audit event) belongs to the caller.
pub trait CustodyBackend: Send + Sync {
    fn kind(&self) -> BackendKind;
    /// Store (or replace) an entry's material.
    fn store(&self, name: &str, material: &[u8]) -> Result<(), CustodyError>;
    /// Retrieve an entry. Fail-closed: every failure is a named error and
    /// serves nothing.
    fn retrieve(&self, name: &str) -> Result<Secret, CustodyError>;
    /// Delete an entry. Deleting an absent entry is `Ok` — deletion is a
    /// desired end state, not an observation.
    fn delete(&self, name: &str) -> Result<(), CustodyError>;
    /// Whether an entry exists, without touching its material.
    fn contains(&self, name: &str) -> Result<bool, CustodyError>;
}
