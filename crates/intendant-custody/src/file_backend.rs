//! The honest floor: plain 0600 files in a private directory. Same-uid
//! readable by construction — callers MUST surface the label (the ruled
//! posture for headless Linux and for source installs pre-#154). Exists
//! so "no keystore" degrades to a *named* posture instead of a pretend
//! one; a passphrase-sealed variant was deliberately not built (the
//! passphrase only recurses the custody question).

use std::path::{Path, PathBuf};

use crate::names::{file_stem_for, validate_entry_name};
use crate::{BackendKind, CustodyBackend, CustodyError, Secret};

pub struct PlainFileBackend {
    dir: PathBuf,
}

impl PlainFileBackend {
    /// Backend over `dir`, created private (0700 on Unix) if absent.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, CustodyError> {
        let dir = dir.into();
        create_private_dir_all(&dir)
            .map_err(|error| CustodyError::io(format!("create {}", dir.display()), error))?;
        Ok(Self { dir })
    }

    fn entry_path(&self, name: &str) -> Result<PathBuf, CustodyError> {
        validate_entry_name(name)?;
        Ok(self.dir.join(format!("{}.plain", file_stem_for(name))))
    }
}

impl CustodyBackend for PlainFileBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::PlainFile
    }

    fn store(&self, name: &str, material: &[u8]) -> Result<(), CustodyError> {
        let path = self.entry_path(name)?;
        write_private_file(&path, material)
            .map_err(|error| CustodyError::io(format!("store {name}"), error))
    }

    fn retrieve(&self, name: &str) -> Result<Secret, CustodyError> {
        let path = self.entry_path(name)?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Secret::new(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(CustodyError::NotFound {
                    name: name.to_string(),
                })
            }
            Err(error) => Err(CustodyError::io(format!("retrieve {name}"), error)),
        }
    }

    fn delete(&self, name: &str) -> Result<(), CustodyError> {
        let path = self.entry_path(name)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(CustodyError::io(format!("delete {name}"), error)),
        }
    }

    fn contains(&self, name: &str) -> Result<bool, CustodyError> {
        Ok(self.entry_path(name)?.exists())
    }
}

/// Private-dir/private-file helpers, local to this leaf crate (pulling the
/// daemon's shared crate here would drag its async runtime along). Unix
/// modes apply at creation; other platforms fall back to default ACLs.
pub(crate) fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(dir)
}

pub(crate) fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    // The mode applies only at creation — remove any pre-existing file so
    // stale permissions can never survive a replace.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_delete_and_modes() {
        let dir = tempfile::tempdir().unwrap();
        let backend = PlainFileBackend::new(dir.path().join("custody")).unwrap();
        assert_eq!(backend.kind(), BackendKind::PlainFile);

        assert!(!backend.contains("access-certs/client.key").unwrap());
        assert!(matches!(
            backend.retrieve("access-certs/client.key"),
            Err(CustodyError::NotFound { .. })
        ));

        backend
            .store("access-certs/client.key", b"PEM BYTES")
            .unwrap();
        assert!(backend.contains("access-certs/client.key").unwrap());
        assert_eq!(
            backend
                .retrieve("access-certs/client.key")
                .unwrap()
                .as_bytes(),
            b"PEM BYTES"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let dir_mode = std::fs::metadata(dir.path().join("custody"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o777, 0o700, "custody dir must be private");
            let file_mode =
                std::fs::metadata(dir.path().join("custody/access-certs__client.key.plain"))
                    .unwrap()
                    .permissions()
                    .mode();
            assert_eq!(file_mode & 0o777, 0o600, "entry file must be private");
        }

        // Replace keeps working (remove-then-create path).
        backend
            .store("access-certs/client.key", b"NEW BYTES")
            .unwrap();
        assert_eq!(
            backend
                .retrieve("access-certs/client.key")
                .unwrap()
                .as_bytes(),
            b"NEW BYTES"
        );

        backend.delete("access-certs/client.key").unwrap();
        assert!(!backend.contains("access-certs/client.key").unwrap());
        // Deleting an absent entry is a desired end state, not an error.
        backend.delete("access-certs/client.key").unwrap();
    }

    #[test]
    fn invalid_names_never_touch_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let backend = PlainFileBackend::new(dir.path().join("custody")).unwrap();
        assert!(matches!(
            backend.store("../escape", b"x"),
            Err(CustodyError::InvalidName { .. })
        ));
        assert!(std::fs::read_dir(dir.path().join("custody"))
            .unwrap()
            .next()
            .is_none());
    }
}
