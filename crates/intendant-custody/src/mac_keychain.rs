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
                .map_err(|error| keychain_error(backend, "store wrapping key", &error))?;
                Ok(key)
            }
            Err(error) if error.code() == ERR_SEC_INTERACTION_NOT_ALLOWED => {
                Err(CustodyError::DeniedNonInteractive {
                    backend,
                    reason: "keychain requires interaction this context cannot provide".to_string(),
                })
            }
            Err(error) => Err(keychain_error(backend, "read wrapping key", &error)),
        }
    }
}

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
