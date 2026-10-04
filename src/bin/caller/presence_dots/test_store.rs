//! Private, restart-safe test-dot intent. This is not an IAM grant or an
//! activation store. A recorded attempt is NEVER treated as permission to POST
//! again; reconciliation may only read provider metadata and update this file.

use super::profile;
use intendant_core::state_paths;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read as _,
    path::{Path, PathBuf},
};

const MAX_JOURNAL_BYTES: u64 = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    Attempted,
    Received,
    Verified,
    NeedsReview,
}

// Serialize only to the owner-only journal; deliberately no Debug.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DotRecord {
    pub(super) dot: String,
    pub(super) root: String,
    pub(super) room: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Journal {
    version: u8,
    pub(super) attempt_id: String,
    pub(super) label: String,
    pub(super) fingerprint: String,
    pub(super) primary_guard: String,
    pub(super) primary_dot: String,
    pub(super) protected_roots: Vec<String>,
    pub(super) phase: Phase,
    pub(super) received: Option<DotRecord>,
    pub(super) provider_status: Option<u16>,
}

pub(super) fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

impl DotRecord {
    pub(super) fn valid(&self) -> bool {
        profile::valid_resource_id(&self.dot)
            && profile::valid_thread_id(&self.root)
            && self.room.as_deref().is_none_or(profile::valid_segment)
    }
}

impl Journal {
    pub(super) fn new(fingerprint: String, primary: &profile::PrimarySnapshot) -> Self {
        let attempt_id = uuid::Uuid::new_v4().to_string();
        Self {
            version: 1,
            label: format!("Intendant Presence E2E {attempt_id}"),
            attempt_id,
            fingerprint,
            primary_guard: primary.selection_guard.clone(),
            primary_dot: primary.identity.dot.clone(),
            protected_roots: vec![
                primary.identity.root.clone(),
                primary.selected_thread.clone(),
            ],
            phase: Phase::Attempted,
            received: None,
            provider_status: None,
        }
    }

    fn valid(&self) -> bool {
        self.version == 1
            && profile::valid_thread_id(&self.attempt_id)
            && self.label == format!("Intendant Presence E2E {}", self.attempt_id)
            && valid_digest(&self.fingerprint)
            && valid_digest(&self.primary_guard)
            && profile::valid_resource_id(&self.primary_dot)
            && self.protected_roots.len() == 2
            && self
                .protected_roots
                .iter()
                .all(|s| profile::valid_thread_id(s))
            && self.received.as_ref().is_none_or(DotRecord::valid)
            && (self.phase != Phase::Verified
                || self.received.as_ref().is_some_and(|dot| self.separate(dot)))
            && self
                .provider_status
                .is_none_or(|s| (100..=599).contains(&s))
    }

    pub(super) fn separate(&self, dot: &DotRecord) -> bool {
        dot.valid() && dot.dot != self.primary_dot && !self.protected_roots.contains(&dot.root)
    }
}

pub(super) struct Store {
    path: PathBuf,
    // Holding this file owns a cross-process advisory lock, not provider or
    // local authority. A second process refuses rather than blocking Tokio.
    _lock: File,
}

pub(super) fn check_private(path: &Path, directory: bool) -> Result<(), &'static str> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("test_dot_state_unavailable"),
    };
    if meta.file_type().is_symlink()
        || if directory {
            !meta.is_dir()
        } else {
            !meta.is_file()
        }
    {
        return Err("test_dot_state_path_refused");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if meta.mode() & 0o077 != 0 || (!directory && meta.nlink() != 1) {
            return Err("test_dot_state_permissions_refused");
        }
    }
    Ok(())
}

impl Store {
    pub(super) fn acquire(root: &Path) -> Result<Self, &'static str> {
        check_private(root, true)?;
        state_paths::create_private_dir_all(root).map_err(|_| "test_dot_state_unavailable")?;
        check_private(root, true)?;
        let lock_path = root.join("test-dot.lock");
        check_private(&lock_path, false)?;
        let mut options = state_paths::private_file_options();
        options.truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let lock = options
            .open(&lock_path)
            .map_err(|_| "test_dot_state_unavailable")?;
        check_private(&lock_path, false)?;
        lock.try_lock()
            .map_err(|_| "test_dot_operation_in_progress")?;
        Ok(Self {
            path: root.join("test-dot.json"),
            _lock: lock,
        })
    }

    pub(super) fn load(&self) -> Result<Option<Journal>, &'static str> {
        Self::load_path(&self.path)
    }

    pub(super) fn read(root: &Path) -> Result<Option<Journal>, &'static str> {
        check_private(root, true)?;
        Self::load_path(&root.join("test-dot.json"))
    }

    fn load_path(path: &Path) -> Result<Option<Journal>, &'static str> {
        check_private(path, false)?;
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("test_dot_state_unavailable"),
        };
        let mut bytes = Vec::new();
        file.take(MAX_JOURNAL_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "test_dot_state_unavailable")?;
        if bytes.len() as u64 > MAX_JOURNAL_BYTES {
            return Err("test_dot_journal_refused");
        }
        let journal: Journal =
            serde_json::from_slice(&bytes).map_err(|_| "test_dot_journal_refused")?;
        if !journal.valid() {
            return Err("test_dot_journal_refused");
        }
        Ok(Some(journal))
    }

    pub(super) fn save(&self, journal: &Journal) -> Result<(), &'static str> {
        if !journal.valid() {
            return Err("test_dot_journal_refused");
        }
        check_private(&self.path, false)?;
        let bytes = serde_json::to_vec(journal).map_err(|_| "test_dot_journal_refused")?;
        crate::file_watcher::atomic_write(&self.path, &bytes)
            .map_err(|_| "test_dot_journal_write_failed")?;
        // Sync the directory entry too on Unix BEFORE allowing a provider
        // mutation. Windows uses the existing synced std-only staging seam.
        #[cfg(unix)]
        File::open(self.path.parent().ok_or("test_dot_state_path_refused")?)
            .and_then(|file| file.sync_all())
            .map_err(|_| "test_dot_journal_write_failed")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn fixture() -> Journal {
        Journal {
            version: 1,
            attempt_id: "00000000-0000-4000-8000-000000000001".into(),
            label: "Intendant Presence E2E 00000000-0000-4000-8000-000000000001".into(),
            fingerprint: "a".repeat(64),
            primary_guard: "b".repeat(64),
            primary_dot: "fixture:primary/private".into(),
            protected_roots: vec!["00000000-0000-7000-8000-000000000002".into(); 2],
            phase: Phase::Attempted,
            received: None,
            provider_status: None,
        }
    }

    #[test]
    fn pending_attempt_survives_restart_and_lock_excludes_other_operations() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        {
            let store = Store::acquire(&root).unwrap();
            assert!(store.load().unwrap().is_none());
            store.save(&fixture()).unwrap();
            assert!(matches!(
                Store::acquire(&root),
                Err("test_dot_operation_in_progress")
            ));
        }
        let store = Store::acquire(&root).unwrap();
        let journal = store.load().unwrap().unwrap();
        assert!(journal.phase == Phase::Attempted);
        assert_eq!(journal.attempt_id, fixture().attempt_id);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            assert_eq!(
                std::fs::metadata(&store.path).unwrap().mode() & 0o777,
                0o600
            );
            assert_eq!(std::fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
        }
    }

    #[test]
    fn corrupt_unknown_or_oversized_journals_cannot_become_new_intent() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::acquire(&temp.path().join("state")).unwrap();
        let mut value = serde_json::to_value(fixture()).unwrap();
        value["phase"] = serde_json::json!("verified");
        for bytes in [
            serde_json::to_vec(&value).unwrap(),
            vec![b'x'; MAX_JOURNAL_BYTES as usize + 1],
            b"broken private data".to_vec(),
        ] {
            crate::file_watcher::atomic_write(&store.path, &bytes).unwrap();
            assert!(matches!(store.load(), Err("test_dot_journal_refused")));
        }
        value = serde_json::to_value(fixture()).unwrap();
        value["unknown"] = serde_json::json!("private scalar");
        crate::file_watcher::atomic_write(&store.path, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        assert!(store.load().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn state_symlink_and_loose_permissions_refuse_without_changing_target() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("protected");
        std::fs::write(&outside, b"unchanged").unwrap();
        let root = temp.path().join("state");
        let store = Store::acquire(&root).unwrap();
        symlink(&outside, &store.path).unwrap();
        assert!(store.load().is_err());
        assert!(store.save(&fixture()).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"unchanged");
        std::fs::remove_file(&store.path).unwrap();
        store.save(&fixture()).unwrap();
        std::fs::set_permissions(&store.path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.load().is_err());
        assert!(store.save(&fixture()).is_err());
    }
}
