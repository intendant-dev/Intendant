//! One fixed text-probe intent, separate from additional-dot creation state.
//! Recorded intent is never permission to retry, reset or enable Presence.

use super::{profile, test_store};
use intendant_core::state_paths;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read as _,
    path::{Path, PathBuf},
};

const MAX_BYTES: u64 = 64 * 1024;
const JOURNAL_NAME: &str = "existing-dot-probe.json";
const LOCK_NAME: &str = "existing-dot-probe.lock";

// Private persistence only. Deliberately no Debug or diagnostic serialization.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Journal {
    version: u8,
    pub(super) attempt: String,
    pub(super) fingerprint: String,
    pub(super) selection_guard: String,
    pub(super) dot: String,
    pub(super) root: String,
    pub(super) room: String,
    pub(super) receipt: Option<String>,
    pub(super) provider_status: Option<u16>,
    pub(super) message_observed: bool,
    pub(super) reply_observed: bool,
}

impl Journal {
    pub(super) fn new(
        fingerprint: String,
        primary: &profile::PrimarySnapshot,
    ) -> Result<Self, &'static str> {
        if !primary.room_linked() {
            return Err("existing_probe_room_not_linked");
        }
        Ok(Self {
            version: 1,
            attempt: uuid::Uuid::new_v4().to_string(),
            fingerprint,
            selection_guard: primary.selection_guard.clone(),
            dot: primary.identity.dot.clone(),
            root: primary.identity.root.clone(),
            room: primary
                .identity
                .room
                .clone()
                .ok_or("existing_probe_room_missing")?,
            receipt: None,
            provider_status: None,
            message_observed: false,
            reply_observed: false,
        })
    }

    fn valid(&self) -> bool {
        self.version == 1
            && profile::valid_thread_id(&self.attempt)
            && test_store::valid_digest(&self.fingerprint)
            && test_store::valid_digest(&self.selection_guard)
            && profile::valid_resource_id(&self.dot)
            && profile::valid_thread_id(&self.root)
            && profile::valid_segment(&self.room)
            && self
                .receipt
                .as_deref()
                .is_none_or(profile::valid_resource_id)
            && self
                .provider_status
                .is_none_or(|s| (100..=599).contains(&s))
            && (!self.message_observed || self.receipt.is_some())
            && (!self.reply_observed || self.message_observed)
    }
}

pub(super) struct Store {
    path: PathBuf,
    _lock: File,
}

impl Store {
    pub(super) fn acquire(root: &Path) -> Result<Self, &'static str> {
        test_store::check_private(root, true)?;
        state_paths::create_private_dir_all(root)
            .map_err(|_| "existing_probe_state_unavailable")?;
        test_store::check_private(root, true)?;
        let path = root.join(LOCK_NAME);
        test_store::check_private(&path, false)?;
        let mut options = state_paths::private_file_options();
        options.truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let lock = options
            .open(&path)
            .map_err(|_| "existing_probe_state_unavailable")?;
        test_store::check_private(&path, false)?;
        lock.try_lock().map_err(|_| "existing_probe_in_progress")?;
        Ok(Self {
            path: root.join(JOURNAL_NAME),
            _lock: lock,
        })
    }

    pub(super) fn load(&self) -> Result<Option<Journal>, &'static str> {
        Self::load_path(&self.path)
    }

    pub(super) fn read(root: &Path) -> Result<Option<Journal>, &'static str> {
        test_store::check_private(root, true)?;
        Self::load_path(&root.join(JOURNAL_NAME))
    }

    fn load_path(path: &Path) -> Result<Option<Journal>, &'static str> {
        test_store::check_private(path, false)?;
        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("existing_probe_state_unavailable"),
        };
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "existing_probe_state_unavailable")?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("existing_probe_journal_refused");
        }
        let journal: Journal =
            serde_json::from_slice(&bytes).map_err(|_| "existing_probe_journal_refused")?;
        if !journal.valid() {
            return Err("existing_probe_journal_refused");
        }
        Ok(Some(journal))
    }

    pub(super) fn save(&self, journal: &Journal) -> Result<(), &'static str> {
        if !journal.valid() {
            return Err("existing_probe_journal_refused");
        }
        test_store::check_private(&self.path, false)?;
        let bytes = serde_json::to_vec(journal).map_err(|_| "existing_probe_journal_refused")?;
        crate::file_watcher::atomic_write(&self.path, &bytes)
            .map_err(|_| "existing_probe_journal_write_failed")?;
        #[cfg(unix)]
        File::open(
            self.path
                .parent()
                .ok_or("existing_probe_state_unavailable")?,
        )
        .and_then(|file| file.sync_all())
        .map_err(|_| "existing_probe_journal_write_failed")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn fixture() -> Journal {
        Journal {
            version: 1,
            attempt: "00000000-0000-7000-8000-000000000009".into(),
            fingerprint: "a".repeat(64),
            selection_guard: "b".repeat(64),
            dot: "fixture:private/dot".into(),
            root: "00000000-0000-7000-8000-000000000001".into(),
            room: "fixture-room".into(),
            receipt: None,
            provider_status: None,
            message_observed: false,
            reply_observed: false,
        }
    }

    #[test]
    fn status_does_not_create_state_or_a_lock() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("missing");
        assert!(Store::read(&root).unwrap().is_none());
        assert!(!root.exists());
    }

    #[test]
    fn lock_and_restart_preserve_intent_without_touching_creation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let store = Store::acquire(&root).unwrap();
        let legacy = root.join("test-dot.json");
        std::fs::write(&legacy, b"protected existing creation intent").unwrap();
        assert!(Store::acquire(&root).is_err());
        store.save(&fixture()).unwrap();
        drop(store);
        let resumed = Store::acquire(&root).unwrap();
        let journal = resumed.load().unwrap().unwrap();
        assert_eq!(journal.attempt, fixture().attempt);
        assert!(journal.receipt.is_none());
        assert_eq!(
            std::fs::read(legacy).unwrap(),
            b"protected existing creation intent"
        );
    }

    #[test]
    fn corrupt_oversized_and_unknown_journals_refuse_without_reset() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let store = Store::acquire(&root).unwrap();
        for bytes in [
            b"not json".to_vec(),
            vec![b' '; MAX_BYTES as usize + 1],
            br#"{"version":2,"unknown":true}"#.to_vec(),
        ] {
            crate::file_watcher::atomic_write(&store.path, &bytes).unwrap();
            assert!(store.load().is_err());
            assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_hardlinks_and_loose_permissions_refuse() {
        use std::os::unix::{
            fs::MetadataExt as _,
            fs::{symlink, PermissionsExt},
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let store = Store::acquire(&root).unwrap();
        store.save(&fixture()).unwrap();
        assert_eq!(std::fs::metadata(&store.path).unwrap().mode() & 0o077, 0);
        std::fs::hard_link(&store.path, temp.path().join("alias")).unwrap();
        assert!(store.load().is_err());
        std::fs::remove_file(temp.path().join("alias")).unwrap();
        std::fs::set_permissions(&store.path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.load().is_err());
        std::fs::remove_file(&store.path).unwrap();
        symlink(temp.path().join("missing"), &store.path).unwrap();
        assert!(store.load().is_err());
    }
}
