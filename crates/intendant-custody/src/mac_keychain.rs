//! macOS wrapping-key provider: one generic-password item in the login
//! keychain holds the 32-byte wrapping key (the ratified Option B+D
//! anchor — the item's ACL records the creating binary, so with a stably
//! signed daemon there are no per-rebuild prompts, and headless callers
//! land on `errSecInteractionNotAllowed` instead of stalling).
//!
//! No test here touches a keychain: the login keychain is off-limits to
//! tests by doctrine, and this provider's discrimination behavior is
//! exactly what the K2 acceptance rig (second helper binary, operator
//! prompt lane) exists to demonstrate. The backend logic above this seam
//! is covered un-gated in `wrapped.rs`.

use zeroize::{Zeroize as _, Zeroizing};

use crate::{BackendKind, CustodyError, WrappedBlobBackend, WrappingKeyProvider};

const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25308;
/// `errSecAuthFailed`. On modern macOS an item-ACL denial with UI
/// disabled surfaces as this code, not `errSecInteractionNotAllowed`
/// (proven live by the acceptance rig on 2026-07-21). It is *also* the
/// wrong-password code, so it maps to the deny class only on item-access
/// operations — an explicit unlock has already succeeded by then, which
/// removes the wrong-password reading.
const ERR_SEC_AUTH_FAILED: i32 = -25293;

const DEFAULT_SERVICE: &str = "dev.intendant.custody";
const DEFAULT_ACCOUNT: &str = "wrapping-key-v1";

pub struct LoginKeychainKeyProvider {
    service: String,
    account: String,
}

impl Default for LoginKeychainKeyProvider {
    fn default() -> Self {
        Self {
            service: DEFAULT_SERVICE.to_string(),
            account: DEFAULT_ACCOUNT.to_string(),
        }
    }
}

impl WrappingKeyProvider for LoginKeychainKeyProvider {
    fn wrapping_key(&self, create_if_missing: bool) -> Result<Zeroizing<[u8; 32]>, CustodyError> {
        let backend = BackendKind::MacKeychainWrapped;
        match security_framework::passwords::get_generic_password(&self.service, &self.account) {
            Ok(mut bytes) => {
                if bytes.len() != 32 {
                    bytes.zeroize();
                    return Err(CustodyError::BackendUnavailable {
                        backend,
                        reason: "wrapping-key keychain item is malformed".to_string(),
                    });
                }
                let mut key = Zeroizing::new([0u8; 32]);
                key.copy_from_slice(&bytes);
                bytes.zeroize();
                Ok(key)
            }
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => {
                if !create_if_missing {
                    return Err(CustodyError::BackendUnavailable {
                        backend,
                        reason: "wrapping key absent from the keychain".to_string(),
                    });
                }
                let mut key = Zeroizing::new([0u8; 32]);
                ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), key.as_mut())
                    .map_err(|_| CustodyError::Io {
                        context: "mint wrapping key".to_string(),
                        message: "system randomness unavailable".to_string(),
                    })?;
                security_framework::passwords::set_generic_password(
                    &self.service,
                    &self.account,
                    key.as_ref(),
                )
                .map_err(|error| item_access_error(backend, "store wrapping key", &error))?;
                Ok(key)
            }
            Err(error) => Err(item_access_error(backend, "read wrapping key", &error)),
        }
    }
}

/// A wrapping-key provider bound to one file-backed keychain at an
/// explicit path — never the login keychain. This is the acceptance-rig
/// provider (Track K ruling Q7): the rig creates a throwaway keychain in
/// a temp dir, and caller discrimination is exercised against it with
/// keychain UI disabled, so the deny leg is deterministic on dev Macs and
/// headless CI listeners alike. The keychain is opened and unlocked with
/// its explicit password on every use (an explicit-credential unlock has
/// no UI), leaving the item ACL as the only interaction surface.
pub struct ScopedKeychainKeyProvider {
    keychain_path: std::path::PathBuf,
    keychain_password: String,
    service: String,
    account: String,
}

impl ScopedKeychainKeyProvider {
    /// Create the file-backed keychain at `path` (no UI, password set
    /// programmatically) and return a provider bound to it.
    pub fn create(
        path: impl Into<std::path::PathBuf>,
        password: &str,
    ) -> Result<Self, CustodyError> {
        let path = path.into();
        security_framework::os::macos::keychain::CreateOptions::new()
            .password(password)
            .prompt_user(false)
            .create(&path)
            .map_err(|error| {
                keychain_error(BackendKind::MacKeychainWrapped, "create keychain", &error)
            })?;
        Ok(Self::open(path, password))
    }

    /// Bind to an existing file-backed keychain without touching it yet.
    pub fn open(path: impl Into<std::path::PathBuf>, password: &str) -> Self {
        Self {
            keychain_path: path.into(),
            keychain_password: password.to_string(),
            service: DEFAULT_SERVICE.to_string(),
            account: DEFAULT_ACCOUNT.to_string(),
        }
    }

    fn unlocked_keychain(
        &self,
    ) -> Result<security_framework::os::macos::keychain::SecKeychain, CustodyError> {
        let backend = BackendKind::MacKeychainWrapped;
        let mut keychain =
            security_framework::os::macos::keychain::SecKeychain::open(&self.keychain_path)
                .map_err(|error| keychain_error(backend, "open keychain", &error))?;
        keychain
            .unlock(Some(&self.keychain_password))
            .map_err(|error| keychain_error(backend, "unlock keychain", &error))?;
        Ok(keychain)
    }
}

impl WrappingKeyProvider for ScopedKeychainKeyProvider {
    fn wrapping_key(&self, create_if_missing: bool) -> Result<Zeroizing<[u8; 32]>, CustodyError> {
        let backend = BackendKind::MacKeychainWrapped;
        let keychain = self.unlocked_keychain()?;
        match keychain.find_generic_password(&self.service, &self.account) {
            Ok((bytes, _item)) => {
                if bytes.len() != 32 {
                    return Err(CustodyError::BackendUnavailable {
                        backend,
                        reason: "wrapping-key keychain item is malformed".to_string(),
                    });
                }
                let mut key = Zeroizing::new([0u8; 32]);
                key.copy_from_slice(&bytes);
                Ok(key)
            }
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => {
                if !create_if_missing {
                    return Err(CustodyError::BackendUnavailable {
                        backend,
                        reason: "wrapping key absent from the keychain".to_string(),
                    });
                }
                let mut key = Zeroizing::new([0u8; 32]);
                ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), key.as_mut())
                    .map_err(|_| CustodyError::Io {
                        context: "mint wrapping key".to_string(),
                        message: "system randomness unavailable".to_string(),
                    })?;
                keychain
                    .add_generic_password(&self.service, &self.account, key.as_ref())
                    .map_err(|error| item_access_error(backend, "store wrapping key", &error))?;
                Ok(key)
            }
            Err(error) => Err(item_access_error(backend, "read wrapping key", &error)),
        }
    }
}

/// RAII guard from [`disable_keychain_ui`]; interaction is re-enabled
/// when it drops.
pub use security_framework::os::macos::keychain::KeychainUserInteractionLock;

/// Disable keychain user interaction for this process — every operation
/// that would show a prompt fails with `errSecInteractionNotAllowed`
/// instead. The acceptance rig runs both its legs under this lock so the
/// deny assertion is a deterministic error, not a hung prompt, in
/// headless contexts.
pub fn disable_keychain_ui() -> Result<KeychainUserInteractionLock, CustodyError> {
    security_framework::os::macos::keychain::SecKeychain::disable_user_interaction().map_err(
        |error| {
            keychain_error(
                BackendKind::MacKeychainWrapped,
                "disable keychain interaction",
                &error,
            )
        },
    )
}

/// Translate errors from keychain *setup* operations (open, unlock,
/// create). Only `errSecInteractionNotAllowed` is the deny class here —
/// `errSecAuthFailed` at this stage means a wrong keychain password, a
/// backend problem, not caller discrimination.
fn keychain_error(
    backend: BackendKind,
    action: &str,
    error: &security_framework::base::Error,
) -> CustodyError {
    if error.code() == ERR_SEC_INTERACTION_NOT_ALLOWED {
        return CustodyError::DeniedNonInteractive {
            backend,
            reason: "keychain requires interaction this context cannot provide".to_string(),
        };
    }
    CustodyError::BackendUnavailable {
        backend,
        reason: format!("{action}: keychain error {}", error.code()),
    }
}

/// Translate errors from *item access* (find/read, add/store). Both
/// `errSecInteractionNotAllowed` and `errSecAuthFailed` are the
/// non-interactive deny class here: any explicit unlock has already
/// succeeded, so an authorization failure is the item ACL refusing this
/// caller without UI.
fn item_access_error(
    backend: BackendKind,
    action: &str,
    error: &security_framework::base::Error,
) -> CustodyError {
    let code = error.code();
    if code == ERR_SEC_INTERACTION_NOT_ALLOWED || code == ERR_SEC_AUTH_FAILED {
        return CustodyError::DeniedNonInteractive {
            backend,
            reason: format!("keychain refused item access without interaction (OSStatus {code})"),
        };
    }
    CustodyError::BackendUnavailable {
        backend,
        reason: format!("{action}: keychain error {code}"),
    }
}

/// The assembled macOS backend: sealed blobs under `dir`, wrapping key in
/// the login keychain.
pub fn mac_wrapped_backend(
    dir: impl Into<std::path::PathBuf>,
) -> Result<WrappedBlobBackend, CustodyError> {
    WrappedBlobBackend::new(
        dir,
        BackendKind::MacKeychainWrapped,
        Box::new(LoginKeychainKeyProvider::default()),
    )
}
