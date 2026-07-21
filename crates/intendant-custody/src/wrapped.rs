//! The wrapped-blob backend: entries persist as sealed blobs on disk;
//! the 32-byte wrapping key lives wherever the [`WrappingKeyProvider`]
//! puts it (macOS: a keychain item ACL'd to the daemon binary — the
//! ratified Option B+D shape, one prompt surface for the whole estate
//! instead of one per entry). The blob-file mechanics and the sealing
//! envelope are platform-independent and tested un-gated; only the
//! provider is platform code.

use std::path::PathBuf;

use ring::rand::SystemRandom;
use zeroize::Zeroizing;

use crate::file_backend::{create_private_dir_all, write_private_file};
use crate::names::sealed_blob_file_name;
use crate::seal::{seal, unseal};
use crate::{BackendKind, CustodyBackend, CustodyError, Secret};

/// Serves (and on first store, mints) the backend's wrapping key. The
/// provider is the caller-discrimination surface: whether retrieval is
/// silent, prompted, or denied is decided here by the platform keystore,
/// never by the blob files.
pub trait WrappingKeyProvider: Send + Sync {
    /// The 32-byte wrapping key. `create_if_missing` is true only on
    /// store paths — retrieval must never mint a key (a fresh key cannot
    /// unseal existing blobs, so minting there would convert "keystore
    /// lost its key" into silent data loss instead of a named failure).
    fn wrapping_key(&self, create_if_missing: bool) -> Result<Zeroizing<[u8; 32]>, CustodyError>;
}

pub struct WrappedBlobBackend {
    dir: PathBuf,
    kind: BackendKind,
    provider: Box<dyn WrappingKeyProvider>,
    rng: SystemRandom,
}

impl WrappedBlobBackend {
    pub fn new(
        dir: impl Into<PathBuf>,
        kind: BackendKind,
        provider: Box<dyn WrappingKeyProvider>,
    ) -> Result<Self, CustodyError> {
        let dir = dir.into();
        create_private_dir_all(&dir)
            .map_err(|error| CustodyError::io(format!("create {}", dir.display()), error))?;
        Ok(Self {
            dir,
            kind,
            provider,
            rng: SystemRandom::new(),
        })
    }

    fn blob_path(&self, name: &str) -> Result<PathBuf, CustodyError> {
        Ok(self.dir.join(sealed_blob_file_name(name)?))
    }
}

impl CustodyBackend for WrappedBlobBackend {
    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn store(&self, name: &str, material: &[u8]) -> Result<(), CustodyError> {
        let path = self.blob_path(name)?;
        let key = self.provider.wrapping_key(true)?;
        let blob = seal(&key, name, material, &self.rng)?;
        write_private_file(&path, &blob)
            .map_err(|error| CustodyError::io(format!("store {name}"), error))
    }

    fn retrieve(&self, name: &str) -> Result<Secret, CustodyError> {
        let path = self.blob_path(name)?;
        let blob = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(CustodyError::NotFound {
                    name: name.to_string(),
                })
            }
            Err(error) => return Err(CustodyError::io(format!("retrieve {name}"), error)),
        };
        // Blob first, key second: an absent entry answers NotFound without
        // ever touching the keystore (no spurious prompt/deny surface).
        let key = self.provider.wrapping_key(false)?;
        unseal(&key, name, &blob)
    }

    fn delete(&self, name: &str) -> Result<(), CustodyError> {
        let path = self.blob_path(name)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(CustodyError::io(format!("delete {name}"), error)),
        }
    }

    fn contains(&self, name: &str) -> Result<bool, CustodyError> {
        Ok(self.blob_path(name)?.exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Test provider: fixed key, optional headless-deny simulation, and a
    /// flag observing whether retrieval ever asked to create — the
    /// platform-shaped behavior, exercised un-gated on every platform.
    struct StaticProvider {
        key: [u8; 32],
        deny: bool,
        created: Arc<AtomicBool>,
    }

    impl WrappingKeyProvider for StaticProvider {
        fn wrapping_key(
            &self,
            create_if_missing: bool,
        ) -> Result<Zeroizing<[u8; 32]>, CustodyError> {
            if self.deny {
                return Err(CustodyError::DeniedNonInteractive {
                    backend: BackendKind::MacKeychainWrapped,
                    reason: "interaction not allowed (simulated)".to_string(),
                });
            }
            if create_if_missing {
                self.created.store(true, Ordering::Relaxed);
            }
            Ok(Zeroizing::new(self.key))
        }
    }

    fn backend(dir: &Path, key: [u8; 32], deny: bool) -> (WrappedBlobBackend, Arc<AtomicBool>) {
        let created = Arc::new(AtomicBool::new(false));
        let backend = WrappedBlobBackend::new(
            dir.join("custody"),
            BackendKind::MacKeychainWrapped,
            Box::new(StaticProvider {
                key,
                deny,
                created: created.clone(),
            }),
        )
        .unwrap();
        (backend, created)
    }

    #[test]
    fn roundtrip_via_provider_and_no_mint_on_retrieve() {
        let dir = tempfile::tempdir().unwrap();
        let (writer, created) = backend(dir.path(), [9u8; 32], false);
        writer
            .store("access-certs/client.key", b"KEY MATERIAL")
            .unwrap();
        assert!(created.load(Ordering::Relaxed), "store may mint the key");

        let (reader, created) = backend(dir.path(), [9u8; 32], false);
        assert_eq!(
            reader
                .retrieve("access-certs/client.key")
                .unwrap()
                .as_bytes(),
            b"KEY MATERIAL"
        );
        assert!(
            !created.load(Ordering::Relaxed),
            "retrieval must never ask to mint a wrapping key"
        );

        // On-disk artifact is a sealed blob, not the material.
        let blob =
            std::fs::read(dir.path().join("custody/access-certs__client.key.sealed")).unwrap();
        assert!(!blob
            .windows(b"KEY MATERIAL".len())
            .any(|w| w == b"KEY MATERIAL"));
    }

    #[test]
    fn wrong_wrapping_key_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let (writer, _) = backend(dir.path(), [1u8; 32], false);
        writer.store("entry", b"material").unwrap();
        let (reader, _) = backend(dir.path(), [2u8; 32], false);
        assert!(matches!(
            reader.retrieve("entry"),
            Err(CustodyError::Unsealable { .. })
        ));
    }

    #[test]
    fn headless_deny_surfaces_named_and_absent_entries_skip_the_keystore() {
        let dir = tempfile::tempdir().unwrap();
        let (writer, _) = backend(dir.path(), [3u8; 32], false);
        writer.store("entry", b"material").unwrap();

        let (denied, _) = backend(dir.path(), [3u8; 32], true);
        assert!(matches!(
            denied.retrieve("entry"),
            Err(CustodyError::DeniedNonInteractive { .. })
        ));
        // An absent entry answers NotFound without consulting the provider
        // — no prompt/deny surface for entries that do not exist.
        assert!(matches!(
            denied.retrieve("absent"),
            Err(CustodyError::NotFound { .. })
        ));
    }
}
