//! Explicit owner-surface profile import into a freshly reserved workspace.
//! A digest is byte identity, not a statement that the profile is safe to use.
use std::fs;
use std::io::Cursor;
#[cfg(target_os = "linux")]
use std::io::Read as _;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use super::{
    extension_policy, BrowserWorkspaceError, BrowserWorkspaceProfileTemplate,
    CreateBrowserWorkspaceRequest,
};

const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;

pub(super) struct ProfileTemplate {
    path: PathBuf,
    sha256: String,
    byte_length: u64,
}

pub(super) fn requested(request: &CreateBrowserWorkspaceRequest) -> bool {
    request.profile_template_archive_path.is_some()
        || request.profile_template_archive_sha256.is_some()
        || request.profile_template_archive_byte_length.is_some()
}

pub(super) fn parse(
    request: &CreateBrowserWorkspaceRequest,
    managed_extension_on_linux_display: bool,
) -> Result<Option<ProfileTemplate>, BrowserWorkspaceError> {
    if !requested(request) {
        return Ok(None);
    }
    let (Some(path), Some(sha256), Some(byte_length)) = (
        request.profile_template_archive_path.as_deref(),
        request.profile_template_archive_sha256.as_deref(),
        request.profile_template_archive_byte_length,
    ) else {
        return Err(invalid(
            "profile-template path, SHA-256 and byte length are required together",
        ));
    };
    if !managed_extension_on_linux_display || !cfg!(target_os = "linux") {
        return Err(invalid(
            "profile templates require a Linux owned-display managed-extension workspace",
        ));
    }
    let path = PathBuf::from(path);
    if !extension_policy::absolute_path_valid(&path)
        || !extension_policy::sha256_valid(sha256)
        || byte_length == 0
        || byte_length > MAX_ARCHIVE_BYTES
    {
        return Err(invalid("invalid profile-template archive identity or size"));
    }
    Ok(Some(ProfileTemplate {
        path,
        sha256: sha256.to_owned(),
        byte_length,
    }))
}

#[cfg(test)]
mod request_tests {
    use super::*;

    #[test]
    fn no_template_preserves_ordinary_request() {
        let request: CreateBrowserWorkspaceRequest = serde_json::from_str("{}").unwrap();
        assert!(!requested(&request));
        assert!(parse(&request, false).unwrap().is_none());
    }

    #[test]
    fn partial_tuple_and_wrong_runtime_refuse() {
        for name in [
            "profile_template_archive_path",
            "profile_template_archive_sha256",
            "profile_template_archive_byte_length",
        ] {
            let value = if name.ends_with("byte_length") {
                serde_json::json!(1)
            } else {
                serde_json::json!("/private/template.zip")
            };
            let request: CreateBrowserWorkspaceRequest =
                serde_json::from_value(serde_json::json!({name:value})).unwrap();
            assert!(requested(&request));
            assert!(parse(&request, true).is_err());
        }
        let request: CreateBrowserWorkspaceRequest = serde_json::from_value(serde_json::json!({
            "profile_template_archive_path":"/private/template.zip",
            "profile_template_archive_sha256":"a".repeat(64),
            "profile_template_archive_byte_length":1
        }))
        .unwrap();
        assert!(parse(&request, false).is_err());
        assert_eq!(parse(&request, true).is_ok(), cfg!(target_os = "linux"));
    }
}

impl ProfileTemplate {
    pub(super) fn materialize(
        &self,
        destination: &Path,
    ) -> Result<BrowserWorkspaceProfileTemplate, BrowserWorkspaceError> {
        let bytes = self.snapshot()?;
        if bytes.len() as u64 != self.byte_length
            || format!("{:x}", Sha256::digest(&bytes)) != self.sha256
        {
            return Err(invalid(
                "profile-template archive bytes do not match their binding",
            ));
        }
        // Reuse the bounded ZIP extractor: exact/case-folded duplicates,
        // traversal, links, devices, oversized entries and expansion refuse.
        let paths = super::extract_browser_extension_archive(Cursor::new(bytes), destination)?;
        if !paths.contains("Local State") || !paths.contains("Default/Preferences") {
            return Err(invalid("profile template must contain exact Chrome Local State and Default/Preferences files"));
        }
        for path in paths {
            if path.split('/').any(|part| part.starts_with("Singleton"))
                || path
                    .split('/')
                    .any(|part| matches!(part, "DevToolsActivePort" | "unlock-password.secret"))
            {
                return Err(invalid(
                    "profile template contains a process lock or unlock sidecar",
                ));
            }
        }
        make_private(destination)?;
        Ok(BrowserWorkspaceProfileTemplate {
            archive_sha256: self.sha256.clone(),
            archive_byte_length: self.byte_length,
            imported_into_fresh_profile: true,
        })
    }

    fn snapshot(&self) -> Result<Vec<u8>, BrowserWorkspaceError> {
        #[cfg(not(target_os = "linux"))]
        {
            return Err(invalid("profile templates require Linux"));
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
            if !extension_policy::absolute_path_valid(&self.path) {
                return Err(invalid("invalid profile-template archive path"));
            }
            let before = fs::symlink_metadata(&self.path)
                .map_err(|_| invalid("profile-template archive unavailable"))?;
            let safe = |m: &fs::Metadata| {
                m.is_file()
                    && m.uid() == intendant_platform::platform::unix_effective_uid()
                    && matches!(m.mode() & 0o7777, 0o400 | 0o600)
                    && m.nlink() == 1
                    && m.len() == self.byte_length
            };
            if !safe(&before) {
                return Err(invalid("profile-template archive must be a private owned regular file of the bound length"));
            }
            let mut file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&self.path)
                .map_err(|_| invalid("cannot open profile-template archive safely"))?;
            let opened = file
                .metadata()
                .map_err(|_| invalid("cannot inspect template handle"))?;
            if !safe(&opened) || opened.dev() != before.dev() || opened.ino() != before.ino() {
                return Err(invalid("profile-template archive changed before opening"));
            }
            let mut bytes = Vec::new();
            file.by_ref()
                .take(self.byte_length + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| invalid("cannot read profile-template archive"))?;
            let after = file
                .metadata()
                .map_err(|_| invalid("cannot inspect template handle after reading"))?;
            if !safe(&after)
                || after.dev() != opened.dev()
                || after.ino() != opened.ino()
                || after.mtime() != opened.mtime()
                || after.mtime_nsec() != opened.mtime_nsec()
                || after.ctime() != opened.ctime()
                || after.ctime_nsec() != opened.ctime_nsec()
            {
                return Err(invalid("profile-template archive changed while reading"));
            }
            Ok(bytes)
        }
    }
}

fn invalid(message: &str) -> BrowserWorkspaceError {
    BrowserWorkspaceError::Unsupported(message.to_owned())
}

fn make_private(path: &Path) -> Result<(), BrowserWorkspaceError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| invalid("cannot inspect imported profile"))?;
    if metadata.file_type().is_symlink() || (!metadata.is_dir() && !metadata.is_file()) {
        return Err(invalid("imported profile contains an unsafe object"));
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path).map_err(|_| invalid("cannot enumerate imported profile"))? {
            make_private(
                &entry
                    .map_err(|_| invalid("cannot inspect imported profile entry"))?
                    .path(),
            )?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if metadata.is_dir() { 0o700 } else { 0o600 }),
        )
        .map_err(|_| invalid("cannot protect imported profile"))?;
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn fixture(entries: &[(&str, &[u8])]) -> (tempfile::TempDir, ProfileTemplate) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("profile.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        for (name, bytes) in entries {
            zip.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let bytes = fs::read(&path).unwrap();
        let spec = ProfileTemplate {
            path,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            byte_length: bytes.len() as u64,
        };
        (root, spec)
    }

    #[test]
    fn exact_template_imports_only_into_empty_destination() {
        let (root, spec) = fixture(&[
            ("Local State", b"{}"),
            ("Default/Preferences", b"{}"),
            (
                "Default/Local Extension Settings/example/000001.ldb",
                b"watch-only",
            ),
        ]);
        let destination = root.path().join("fresh");
        fs::create_dir(&destination).unwrap();
        spec.materialize(&destination).unwrap();
        assert_eq!(
            fs::read(destination.join("Default/Local Extension Settings/example/000001.ldb"))
                .unwrap(),
            b"watch-only"
        );
        assert!(spec.materialize(&destination).is_err());
    }

    #[test]
    fn mismatched_archive_does_not_write() {
        let (root, mut spec) = fixture(&[("Local State", b"{}"), ("Default/Preferences", b"{}")]);
        spec.sha256 = "0".repeat(64);
        let destination = root.path().join("fresh");
        fs::create_dir(&destination).unwrap();
        assert!(spec.materialize(&destination).is_err());
        assert_eq!(fs::read_dir(destination).unwrap().count(), 0);
    }

    #[test]
    fn active_lock_and_unlock_sidecar_refuse() {
        for forbidden in [
            "SingletonLock",
            "DevToolsActivePort",
            "Default/unlock-password.secret",
        ] {
            let (root, spec) = fixture(&[
                ("Local State", b"{}"),
                ("Default/Preferences", b"{}"),
                (forbidden, b"not permitted"),
            ]);
            let destination = root.path().join("fresh");
            fs::create_dir(&destination).unwrap();
            assert!(spec.materialize(&destination).is_err());
        }
    }
}
