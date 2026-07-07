//! Persistent daemon identity for browser-control sessions.
//!
//! This is deliberately separate from the TLS certificate identity. TLS certs
//! bind a browser or peer transport to a reachable network endpoint; this key
//! binds an Intendant daemon installation to a stable per-user, per-machine
//! signing key that survives IP and certificate rotation.

use base64::Engine as _;
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const IDENTITY_DIR: &str = "daemon-identity";
const ED25519_PKCS8_FILE: &str = "ed25519.pk8";

#[derive(Clone)]
pub struct DaemonIdentity {
    key_pair: Arc<Ed25519KeyPair>,
}

impl DaemonIdentity {
    pub fn load_or_create_default() -> Result<Self, String> {
        Self::load_or_create(default_identity_path())
    }

    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let parent = path
                    .parent()
                    .ok_or_else(|| format!("identity path has no parent: {}", path.display()))?;
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create identity dir {}: {e}", parent.display()))?;
                let rng = ring::rand::SystemRandom::new();
                let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
                    .map_err(|_| "generate daemon identity key".to_string())?;
                write_private_key(path, pkcs8.as_ref())?;
                pkcs8.as_ref().to_vec()
            }
            Err(e) => return Err(format!("read daemon identity {}: {e}", path.display())),
        };
        Self::from_pkcs8(&bytes)
    }

    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, String> {
        let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8)
            .map_err(|_| "parse daemon identity key".to_string())?;
        Ok(Self {
            key_pair: Arc::new(key_pair),
        })
    }

    pub fn public_key_bytes(&self) -> &[u8] {
        self.key_pair.public_key().as_ref()
    }

    pub fn public_key_b64u(&self) -> String {
        b64u(self.public_key_bytes())
    }

    pub fn sign_b64u(&self, payload: &[u8]) -> String {
        b64u(self.key_pair.sign(payload).as_ref())
    }
}

#[cfg(test)]
pub fn verify_b64u(public_key_b64u: &str, payload: &[u8], signature_b64u: &str) -> bool {
    let Ok(public_key) = b64u_decode(public_key_b64u) else {
        return false;
    };
    let Ok(signature) = b64u_decode(signature_b64u) else {
        return false;
    };
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
        .verify(payload, &signature)
        .is_ok()
}

pub fn b64u(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
fn b64u_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s)
}

/// The daemon-identity state directory — also home to small identity-
/// adjacent records (the signed Connect claim acknowledgment) so they
/// live and die with the key they relate to.
pub fn default_identity_dir() -> PathBuf {
    dirs::data_dir()
        .map(|d| d.join("intendant").join(IDENTITY_DIR))
        .unwrap_or_else(|| std::env::temp_dir().join("intendant").join(IDENTITY_DIR))
}

fn default_identity_path() -> PathBuf {
    default_identity_dir().join(ED25519_PKCS8_FILE)
}

#[cfg(unix)]
fn write_private_key(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("create daemon identity {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("write daemon identity {}: {e}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_key(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes)
        .map_err(|e| format!("write daemon identity {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_persists_and_signatures_verify() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.pk8");
        let first = DaemonIdentity::load_or_create(&path).unwrap();
        let second = DaemonIdentity::load_or_create(&path).unwrap();

        assert_eq!(first.public_key_b64u(), second.public_key_b64u());
        let payload = b"intendant-dashboard-control-v1\nsession\n";
        let sig = first.sign_b64u(payload);
        assert!(verify_b64u(&second.public_key_b64u(), payload, &sig));
        assert!(!verify_b64u(
            &second.public_key_b64u(),
            b"different payload",
            &sig
        ));
    }
}
