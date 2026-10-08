//! Explicit owner-surface profile import into a freshly reserved workspace.
//! A digest is byte identity, not a statement that the profile is safe to use.
#[cfg(target_os = "linux")]
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
    // Parsing rejects this feature off Linux; only the Linux snapshot uses it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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
        // Pin the owned empty reservation before reading any private archive
        // bytes. Extraction resolves through this retained fd, not a pathname
        // another user can replace beneath a writable parent.
        #[cfg(target_os = "linux")]
        let pinned = PinnedDestination::open(destination)?;
        let result = self.materialize_contents(
            destination,
            #[cfg(target_os = "linux")]
            &pinned,
        );
        #[cfg(target_os = "linux")]
        if result.is_err() {
            // Even a renamed reservation stays ours through the retained fd.
            // Do not leave private Chrome state stranded outside the guard's
            // original pathname when extraction or final identity checks fail.
            pinned.clean_contents()?;
        }
        result
    }

    fn materialize_contents(
        &self,
        destination: &Path,
        #[cfg(target_os = "linux")] pinned: &PinnedDestination,
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
        #[cfg(target_os = "linux")]
        let extraction_root = pinned.fd_path();
        #[cfg(not(target_os = "linux"))]
        let extraction_root = destination.to_path_buf();
        let paths = super::extract_browser_extension_archive(Cursor::new(bytes), &extraction_root)?;
        if !paths.contains("Local State") || !paths.contains("Default/Preferences") {
            return Err(invalid("profile template must contain exact Chrome Local State and Default/Preferences files"));
        }
        for path in &paths {
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
        // The extractor creates every file as 0600 and directory as 0700.
        // Keep the root fd alive through all validation and refuse launch if
        // its original path was replaced, even if extraction itself succeeded.
        #[cfg(target_os = "linux")]
        pinned.verify_path(destination)?;
        Ok(BrowserWorkspaceProfileTemplate {
            archive_sha256: self.sha256.clone(),
            archive_byte_length: self.byte_length,
            imported_into_fresh_profile: true,
        })
    }

    fn snapshot(&self) -> Result<Vec<u8>, BrowserWorkspaceError> {
        #[cfg(not(target_os = "linux"))]
        {
            Err(invalid("profile templates require Linux"))
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

#[cfg(target_os = "linux")]
struct PinnedDestination(fs::File);

#[cfg(target_os = "linux")]
impl PinnedDestination {
    fn open(path: &Path) -> Result<Self, BrowserWorkspaceError> {
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
        // Chrome will later launch by path. Third-party writable non-sticky
        // ancestors could rename an owned child even with its private mode.
        for ancestor in path.ancestors().skip(1) {
            let m = fs::symlink_metadata(ancestor)
                .map_err(|_| invalid("cannot inspect profile destination ancestry"))?;
            if !m.is_dir()
                || m.file_type().is_symlink()
                || (m.mode() & 0o022 != 0 && m.mode() & 0o1000 == 0)
            {
                return Err(invalid(
                    "profile-template destination requires stable non-writable or sticky ancestors",
                ));
            }
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| invalid("cannot pin profile-template destination"))?;
        let metadata = file
            .metadata()
            .map_err(|_| invalid("cannot inspect destination fd"))?;
        if !metadata.is_dir()
            || metadata.uid() != intendant_platform::platform::unix_effective_uid()
            || metadata.mode() & 0o7777 != 0o700
        {
            return Err(invalid(
                "profile-template destination must be an owned private directory",
            ));
        }
        let pinned = Self(file);
        pinned.verify_path(path)?;
        if fs::read_dir(pinned.fd_path())
            .map_err(|_| invalid("cannot inspect pinned destination entries"))?
            .next()
            .is_some()
        {
            return Err(invalid("profile-template destination must be empty"));
        }
        Ok(pinned)
    }

    fn fd_path(&self) -> PathBuf {
        use std::os::fd::AsRawFd as _;
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
    }

    fn verify_path(&self, path: &Path) -> Result<(), BrowserWorkspaceError> {
        use std::os::unix::fs::MetadataExt as _;
        let held = self
            .0
            .metadata()
            .map_err(|_| invalid("cannot inspect destination fd"))?;
        let named =
            fs::symlink_metadata(path).map_err(|_| invalid("profile destination was removed"))?;
        if !named.is_dir()
            || named.file_type().is_symlink()
            || named.dev() != held.dev()
            || named.ino() != held.ino()
            || held.uid() != intendant_platform::platform::unix_effective_uid()
            || held.mode() & 0o7777 != 0o700
        {
            return Err(invalid("profile-template destination identity changed"));
        }
        Ok(())
    }

    fn clean_contents(&self) -> Result<(), BrowserWorkspaceError> {
        // The pin admitted an empty, private, owned directory. Only the newly
        // imported attempt data is removed; never follow its replaced path.
        let entries = fs::read_dir(self.fd_path())
            .map_err(|_| invalid("cannot enumerate owned failed profile import for cleanup"))?;
        for entry in entries {
            let path = entry
                .map_err(|_| invalid("cannot inspect owned failed import entry"))?
                .path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| invalid("cannot inspect owned failed import object"))?;
            let result = if metadata.is_dir() && !metadata.file_type().is_symlink() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            result.map_err(|_| {
                invalid(
                    "owned failed profile import cleanup incomplete; manual inspection required",
                )
            })?;
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};

    fn fresh(path: &Path) {
        fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    }

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
        fresh(&destination);
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
        fresh(&destination);
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
            fresh(&destination);
            assert!(spec.materialize(&destination).is_err());
        }
    }

    #[test]
    fn replaced_destination_cannot_redirect_private_extraction() {
        use std::os::unix::fs::symlink;
        let (root, spec) = fixture(&[
            ("Local State", b"private-state"),
            ("Default/Preferences", b"private-prefs"),
        ]);
        let destination = root.path().join("fresh");
        fresh(&destination);
        let pinned = PinnedDestination::open(&destination).unwrap();
        let moved = root.path().join("renamed-owned-reservation");
        fs::rename(&destination, &moved).unwrap();
        let attacker = root.path().join("attacker");
        fs::create_dir(&attacker).unwrap();
        symlink(&attacker, &destination).unwrap();
        super::super::extract_browser_extension_archive(
            Cursor::new(spec.snapshot().unwrap()),
            &pinned.fd_path(),
        )
        .unwrap();
        assert_eq!(fs::read_dir(&attacker).unwrap().count(), 0);
        assert_eq!(
            fs::metadata(moved.join("Local State")).unwrap().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(moved.join("Default")).unwrap().mode() & 0o777,
            0o700
        );
        assert!(pinned.verify_path(&destination).is_err());
        pinned.clean_contents().unwrap();
        assert_eq!(fs::read_dir(&moved).unwrap().count(), 0);
        assert_eq!(fs::read_dir(&attacker).unwrap().count(), 0);
        assert!(spec.materialize(&destination).is_err());
    }

    #[test]
    fn import_refuses_unstable_writable_ancestry_before_reading_archive() {
        use std::os::unix::fs::PermissionsExt;
        let (root, mut spec) = fixture(&[
            ("Local State", b"private"),
            ("Default/Preferences", b"private"),
        ]);
        let parent = root.path().join("shared");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o777)).unwrap();
        let destination = parent.join("fresh");
        fresh(&destination);
        spec.path = root.path().join("missing-archive.zip");
        let error = spec.materialize(&destination).unwrap_err().to_string();
        assert!(error.contains("ancestors"));
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }
}
