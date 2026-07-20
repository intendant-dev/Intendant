//! Per-supervisor Kimi data-home bridge.
//!
//! Kimi's server lock, bearer token, event journal, and MCP configuration all
//! live below `KIMI_CODE_HOME`. Pointing every Intendant wrapper at the user's
//! primary home would therefore make unrelated sessions fight over one server
//! and would require mutating the user's `mcp.json`. Instead each Intendant
//! session gets a stable bridge home. The bridge mirrors the user's Kimi data
//! (auth, config, sessions, skills, plugins, and caches), but owns the
//! server-private files and a merged, token-free MCP declaration.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const BRIDGE_PARENT: &str = "intendant-bridges";
const SYNC_LOCK_NAME: &str = ".intendant-bridge-sync.lock";
const SESSION_INDEX_NAME: &str = "session_index.jsonl";
const PRIVATE_NAMES: &[&str] = &["mcp.json", "server.token", "server", SYNC_LOCK_NAME];
const HISTORY_NAMES: &[&str] = &["sessions", SESSION_INDEX_NAME];
const SYNC_LOCK_TIMEOUT: Duration = Duration::from_secs(15);
const KIMI_CREDENTIAL_PATH: &str = "credentials/kimi-code.json";
const MAX_KIMI_CREDENTIAL_BYTES: u64 = 64 * 1024;
const CREDENTIAL_REFRESH_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone)]
pub(crate) struct BridgeMcpConfig {
    pub(crate) server_name: String,
    pub(crate) url: String,
    pub(crate) bearer_token_env_var: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialRefreshSync {
    Unchanged,
    Updated,
    SourceChanged,
}

/// Compare-and-swap state for one refreshable credential mirrored by copy.
///
/// A normal Unix bridge symlinks `credentials/` and needs no monitor. Windows
/// commonly falls back to a real copy; Kimi's OAuth provider may then rotate
/// its refresh grant in the bridge, invalidating the primary copy. This state
/// adopts only a bridge change whose primary still byte-matches the last value
/// synchronized by this monitor. A logout, login, or concurrent refresh
/// detaches the mirror permanently rather than resurrecting or overwriting
/// authority.
pub(super) struct CredentialRefreshMirror {
    primary_home: PathBuf,
    bridge_home: PathBuf,
    primary_credential: PathBuf,
    bridge_credential: PathBuf,
    synchronized_digest: [u8; 32],
    detached: bool,
}

/// Live poller for copy-fallback OAuth rotation. The polling window bounds the
/// amount of refreshed authority that can exist only in a bridge if the
/// controller is abruptly killed; graceful shutdown always performs one final
/// synchronized pass after the child process has stopped.
pub(super) struct CredentialRefreshMonitor {
    state: std::sync::Arc<std::sync::Mutex<CredentialRefreshMirror>>,
    handle: tokio::task::JoinHandle<()>,
}

impl CredentialRefreshMonitor {
    pub(super) fn prepare(
        primary_home: &Path,
        bridge_home: &Path,
    ) -> io::Result<Option<CredentialRefreshMirror>> {
        prepare_credential_refresh_mirror(primary_home, bridge_home)
    }

    pub(super) fn start(
        mirror: CredentialRefreshMirror,
        event_tx: tokio::sync::mpsc::UnboundedSender<super::AgentEvent>,
    ) -> Self {
        let state = std::sync::Arc::new(std::sync::Mutex::new(mirror));
        let task_state = std::sync::Arc::clone(&state);
        let handle = tokio::spawn(async move {
            let mut reported_error = false;
            loop {
                tokio::time::sleep(CREDENTIAL_REFRESH_POLL_INTERVAL).await;
                let state = std::sync::Arc::clone(&task_state);
                let result = tokio::task::spawn_blocking(move || sync_refresh_state(&state))
                    .await
                    .map_err(|error| {
                        io::Error::other(format!("Kimi credential refresh task panicked: {error}"))
                    })
                    .and_then(|result| result);
                match result {
                    Ok(CredentialRefreshSync::Unchanged | CredentialRefreshSync::Updated) => {
                        reported_error = false;
                    }
                    Ok(CredentialRefreshSync::SourceChanged) => {
                        let _ = event_tx.send(super::AgentEvent::Log {
                            level: "warn".into(),
                            message: "Kimi credential source changed while a copy-fallback bridge \
                                      was active; refusing to overwrite it with refreshed bridge \
                                      authority"
                                .into(),
                        });
                        break;
                    }
                    Err(error) => {
                        if !reported_error {
                            let _ = event_tx.send(super::AgentEvent::Log {
                                level: "warn".into(),
                                message: format!(
                                    "Could not persist a Kimi copy-fallback OAuth refresh: {error}"
                                ),
                            });
                            reported_error = true;
                        }
                    }
                }
            }
        });
        Self { state, handle }
    }

    pub(super) async fn shutdown(mut self) -> io::Result<()> {
        self.handle.abort();
        let _ = (&mut self.handle).await;
        let state = std::sync::Arc::clone(&self.state);
        tokio::task::spawn_blocking(move || sync_refresh_state(&state).map(|_| ()))
            .await
            .map_err(|error| {
                io::Error::other(format!("Kimi credential sync task panicked: {error}"))
            })?
    }

    pub(super) fn sync_on_drop(self) {
        self.handle.abort();
        let _ = sync_refresh_state(&self.state);
    }
}

fn sync_refresh_state(
    state: &std::sync::Arc<std::sync::Mutex<CredentialRefreshMirror>>,
) -> io::Result<CredentialRefreshSync> {
    state
        .lock()
        .map_err(|_| io::Error::other("Kimi credential refresh lock poisoned"))?
        .sync_once()
}

fn prepare_credential_refresh_mirror(
    primary_home: &Path,
    bridge_home: &Path,
) -> io::Result<Option<CredentialRefreshMirror>> {
    validate_managed_bridge_path(bridge_home)?;
    let primary_credential = primary_home.join(KIMI_CREDENTIAL_PATH);
    let bridge_credential = bridge_home.join(KIMI_CREDENTIAL_PATH);
    let primary_present = match fs::symlink_metadata(&primary_credential) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    let bridge_present = match fs::symlink_metadata(&bridge_credential) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    if !primary_present {
        // A logged-out primary is authoritative. Never recover a credential
        // merely because an older bridge still has one.
        return Ok(None);
    }
    if !bridge_present {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Kimi bridge omitted the primary credential",
        ));
    }
    if same_canonical_path(&primary_credential, &bridge_credential) {
        return Ok(None);
    }

    let primary_parent = primary_credential
        .parent()
        .ok_or_else(|| io::Error::other("Kimi primary credential has no parent"))?;
    let bridge_parent = bridge_credential
        .parent()
        .ok_or_else(|| io::Error::other("Kimi bridge credential has no parent"))?;
    require_real_directory(primary_parent, "Kimi primary credential directory")?;
    require_real_directory(bridge_parent, "Kimi bridge credential directory")?;
    let canonical_primary_home = fs::canonicalize(primary_home)?;
    let canonical_primary_parent = fs::canonicalize(primary_parent)?;
    let canonical_bridge = fs::canonicalize(bridge_home)?;
    let canonical_bridge_parent = fs::canonicalize(bridge_parent)?;
    if canonical_primary_parent.parent() != Some(canonical_primary_home.as_path())
        || canonical_bridge_parent.parent() != Some(canonical_bridge.as_path())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Kimi credential copy escaped its expected home",
        ));
    }

    let canonical_primary_credential =
        canonical_primary_parent.join(primary_credential.file_name().unwrap_or_default());
    let canonical_bridge_credential =
        canonical_bridge_parent.join(bridge_credential.file_name().unwrap_or_default());
    let mut primary =
        read_regular_credential(&canonical_primary_credential, "Kimi primary credential")?;
    let mut bridge =
        read_regular_credential(&canonical_bridge_credential, "Kimi bridge credential")?;
    let primary_digest = credential_digest(&primary);
    let bridge_digest = credential_digest(&bridge);
    primary.fill(0);
    bridge.fill(0);
    if primary_digest != bridge_digest {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Kimi credential copy did not match its primary baseline",
        ));
    }

    Ok(Some(CredentialRefreshMirror {
        primary_home: canonical_primary_home,
        bridge_home: canonical_bridge,
        primary_credential: canonical_primary_credential,
        bridge_credential: canonical_bridge_credential,
        synchronized_digest: primary_digest,
        detached: false,
    }))
}

impl CredentialRefreshMirror {
    fn sync_once(&mut self) -> io::Result<CredentialRefreshSync> {
        if self.detached {
            return Ok(CredentialRefreshSync::SourceChanged);
        }
        validate_managed_bridge_path(&self.bridge_home)?;
        let mut bridge =
            read_regular_credential(&self.bridge_credential, "Kimi bridge credential")?;
        let mut bridge_digest = credential_digest(&bridge);
        if bridge_digest == self.synchronized_digest {
            bridge.fill(0);
            return Ok(CredentialRefreshSync::Unchanged);
        }

        let _lock = BridgeSyncLock::acquire(&self.primary_home)?;
        // Another supervised session may have held the lock long enough for
        // Kimi to rotate this bridge again. Adopt the newest complete file.
        bridge.fill(0);
        bridge = read_regular_credential(&self.bridge_credential, "Kimi bridge credential")?;
        bridge_digest = credential_digest(&bridge);
        if bridge_digest == self.synchronized_digest {
            bridge.fill(0);
            return Ok(CredentialRefreshSync::Unchanged);
        }

        let mut primary =
            match read_regular_credential(&self.primary_credential, "Kimi primary credential") {
                Ok(primary) => primary,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::InvalidData
                    ) =>
                {
                    bridge.fill(0);
                    self.detached = true;
                    return Ok(CredentialRefreshSync::SourceChanged);
                }
                Err(error) => {
                    bridge.fill(0);
                    return Err(error);
                }
            };
        let primary_digest = credential_digest(&primary);
        primary.fill(0);
        if primary_digest != self.synchronized_digest {
            bridge.fill(0);
            self.detached = true;
            return Ok(CredentialRefreshSync::SourceChanged);
        }

        // Confirm the CAS input immediately before staging the replacement.
        // Intendant instances serialize on BridgeSyncLock. An unrelated
        // process can still race this tiny check/rename window, but a changed
        // or removed source observed at either read is never recreated.
        let mut confirmation =
            read_regular_credential(&self.primary_credential, "Kimi primary credential")?;
        let confirmed_digest = credential_digest(&confirmation);
        confirmation.fill(0);
        if confirmed_digest != self.synchronized_digest {
            bridge.fill(0);
            self.detached = true;
            return Ok(CredentialRefreshSync::SourceChanged);
        }

        replace_private_credential(&self.primary_credential, &bridge)?;
        let mut installed =
            read_regular_credential(&self.primary_credential, "refreshed Kimi credential")?;
        let installed_digest = credential_digest(&installed);
        installed.fill(0);
        bridge.fill(0);
        if installed_digest != bridge_digest {
            return Err(io::Error::other(
                "Kimi credential refresh failed post-write verification",
            ));
        }
        self.synchronized_digest = bridge_digest;
        Ok(CredentialRefreshSync::Updated)
    }
}

fn read_regular_credential(path: &Path, label: &str) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    reject_non_symlink_reparse(path, &metadata, label)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} {} is not a regular file", path.display()),
        ));
    }
    if metadata.len() > MAX_KIMI_CREDENTIAL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} {} exceeds the size limit", path.display()),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    fs::File::open(path)?
        .take(MAX_KIMI_CREDENTIAL_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_KIMI_CREDENTIAL_BYTES {
        bytes.fill(0);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} {} exceeds the size limit", path.display()),
        ));
    }
    Ok(bytes)
}

fn credential_digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn replace_private_credential(path: &Path, content: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Kimi credential has no parent"))?;
    require_real_directory(parent, "Kimi primary credential directory")?;
    let (mut file, staged) = crate::file_watcher::stage_in(parent)?;
    let staged_write = file.write_all(content).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = staged_write {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    if let Err(error) = set_private_file_permissions(&staged) {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    let result = (|| {
        let metadata = fs::symlink_metadata(path)?;
        reject_non_symlink_reparse(path, &metadata, "Kimi primary credential")?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Kimi primary credential changed before refresh replacement",
            ));
        }
        crate::file_watcher::persist_staged(&staged, path)?;
        set_private_file_permissions(path)?;
        sync_parent_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(staged);
    }
    result
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Pick the conventional `intendant` name unless a higher-precedence project
/// MCP file already owns it. Kimi merges user, project-root, then project-local
/// declarations, so blindly writing only the user-level bridge entry would let
/// a checkout silently replace Intendant's scoped control plane.
pub(crate) fn choose_mcp_server_name(cwd: &Path, _identity: &str) -> io::Result<String> {
    let mut names = std::collections::HashSet::new();
    for path in project_mcp_paths(cwd) {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if text.trim().is_empty() {
            continue;
        }
        let value = serde_json::from_str::<Value>(&text).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid {}: {error}", path.display()),
            )
        })?;
        let object = value.as_object().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} must contain a JSON object", path.display()),
            )
        })?;
        let Some(servers) = object.get("mcpServers") else {
            continue;
        };
        let servers = servers.as_object().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} field mcpServers must contain a JSON object",
                    path.display()
                ),
            )
        })?;
        names.extend(servers.keys().cloned());
    }
    if !names.contains("intendant") {
        return Ok("intendant".into());
    }
    // Kimi persists the active tool names in session state. A suffix derived
    // from the Intendant wrapper id changes across resume/fork bridges and
    // turns every previously selected managed MCP tool into an unknown name.
    // Scope the collision fallback to the project instead: bridge homes remain
    // isolated, so all wrappers for one project can safely use the same name.
    let project_scope = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let suffix = bridge_key(&project_scope.to_string_lossy())
        .strip_prefix("session-")
        .unwrap_or("managed")
        .chars()
        .take(8)
        .collect::<String>();
    let base = format!("intendant_managed_{suffix}");
    if !names.contains(&base) {
        return Ok(base);
    }
    for index in 2u32.. {
        let candidate = format!("{base}_{index}");
        if !names.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err(io::Error::other("Kimi MCP server-name space exhausted"))
}

fn project_mcp_paths(cwd: &Path) -> Vec<PathBuf> {
    let mut paths = vec![cwd.join(".kimi-code").join("mcp.json")];
    let mut cursor = Some(cwd);
    while let Some(directory) = cursor {
        if directory.join(".git").exists() {
            paths.push(directory.join(".mcp.json"));
            break;
        }
        cursor = directory.parent();
    }
    paths
}

/// Prepare a stable bridge rooted below `primary_home`.
///
/// `identity` is the Intendant session id in production. It is hashed rather
/// than embedded in a path both to keep arbitrary ids filesystem-safe and to
/// avoid exposing a potentially descriptive session id in process listings.
pub(crate) fn prepare_bridge_home(
    primary_home: &Path,
    identity: &str,
    mcp: Option<&BridgeMcpConfig>,
) -> io::Result<PathBuf> {
    fs::create_dir_all(primary_home)?;
    let bridge_parent = primary_home.join(BRIDGE_PARENT);
    ensure_private_managed_directory(&bridge_parent)?;
    let bridge = bridge_parent.join(bridge_key(identity));
    ensure_private_managed_directory(&bridge)?;
    validate_managed_bridge_path(&bridge)?;

    // Publish history from every existing bridge before this bridge mirrors
    // the primary home. The first supervised Kimi process can start before
    // `sessions/` exists in the primary home, so that process necessarily
    // creates a real copy-fallback directory in its bridge even on Unix. A
    // concurrent resume/fork bridge must see that live parent history; waiting
    // for the parent's Drop-time sync makes native fork-at-head unverifiable.
    // The sweep is history-only and serialized. Session transcripts retain
    // strict append-only ordering, while Kimi's session index is merged by its
    // actual per-session map semantics. Both ignore incomplete source tails,
    // so copying a live bridge neither exposes private server/auth state nor
    // publishes a partial journal record.
    sync_managed_bridges_to_primary(primary_home)?;
    prune_stale_bridge_snapshots(primary_home, &bridge)?;

    for entry in fs::read_dir(primary_home)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == OsStr::new(BRIDGE_PARENT)
            || name == OsStr::new(SESSION_INDEX_NAME)
            || private_name(&name)
        {
            continue;
        }
        mirror_entry(&entry.path(), &bridge.join(&name))?;
    }
    let primary_index = primary_home.join(SESSION_INDEX_NAME);
    if fs::symlink_metadata(&primary_index).is_ok() {
        let bridge_index = bridge.join(SESSION_INDEX_NAME);
        if fs::symlink_metadata(&bridge_index)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            // Older Intendant builds mirrored the index as a symlink. Kimi's
            // index entries contain absolute session paths, so each bridge
            // needs a materialized, path-rebased copy instead.
            remove_entry_no_follow(&bridge_index)?;
        }
        merge_session_index(&primary_index, &bridge_index, &bridge)?;
    }

    write_merged_mcp(primary_home, &bridge, mcp)?;
    Ok(bridge)
}

/// Persist native session-history copy fallbacks back into the user's primary
/// Kimi home. This is a no-op for the normal symlink-backed layout.
///
/// Windows commonly denies symlink creation outside Developer Mode. In that
/// case Kimi writes its session history into the bridge copies. Copy-back is a
/// strict allowlist: a bridge also contains credential/config/plugin snapshots,
/// and replaying any of those after the user changed or deleted the primary
/// copy could resurrect stale authority. Never follow source or destination
/// symlinks, never delete primary data, and never replace a newer primary file
/// that may belong to another concurrently supervised Kimi process.
pub(crate) fn sync_bridge_home_to_primary(bridge: &Path) -> io::Result<()> {
    validate_managed_bridge_path(bridge)?;
    let bridge_parent = bridge
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Kimi bridge has no parent"))?;
    if bridge_parent.file_name() != Some(OsStr::new(BRIDGE_PARENT)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Kimi bridge is outside the managed bridge directory",
        ));
    }
    let primary_home = bridge_parent.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Kimi bridge directory has no primary home",
        )
    })?;
    let _lock = BridgeSyncLock::acquire(primary_home)?;

    let sessions = bridge.join("sessions");
    if fs::symlink_metadata(&sessions).is_ok() {
        sync_copy_back(&sessions, &primary_home.join("sessions"))?;
    }
    let session_index = bridge.join(SESSION_INDEX_NAME);
    if let Ok(metadata) = fs::symlink_metadata(&session_index) {
        if !metadata.file_type().is_symlink() {
            let primary_index = primary_home.join(SESSION_INDEX_NAME);
            match fs::symlink_metadata(&primary_index) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    // Never write through an authority-controlled primary-home
                    // link. This matches the copy-back policy for all other
                    // history entries.
                }
                Ok(_) => {
                    merge_session_index(&session_index, &primary_index, primary_home)?;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    merge_session_index(&session_index, &primary_index, primary_home)?;
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

/// Flush every managed bridge below a primary Kimi home.
///
/// Credential-lease cleanup calls this before staging `sessions/` and
/// deleting the leased home. Only `sessions/` and `session_index.jsonl` are
/// eligible; credentials, configuration, plugins, caches, server state, MCP
/// configuration, and the bridge lock all remain excluded by
/// [`sync_bridge_home_to_primary`].
pub(crate) fn sync_managed_bridges_to_primary(primary_home: &Path) -> io::Result<()> {
    let parent = primary_home.join(BRIDGE_PARENT);
    match fs::symlink_metadata(&parent) {
        Ok(_) => require_real_directory(&parent, "Kimi managed bridge parent")?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    let entries = match fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut first_error = None;
    for entry in entries {
        let result = (|| {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && entry.file_name().to_string_lossy().starts_with("session-")
            {
                sync_bridge_home_to_primary(&entry.path())?;
            }
            Ok::<(), io::Error>(())
        })();
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

struct BridgeSyncLock {
    file: fs::File,
}

impl BridgeSyncLock {
    fn acquire(primary_home: &Path) -> io::Result<Self> {
        fs::create_dir_all(primary_home)?;
        let path = primary_home.join(SYNC_LOCK_NAME);
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                set_private_file_permissions(&path)?;
                file
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&path)?;
                if crate::platform::path_leaf_is_symlink_or_reparse(&path)?
                    || !metadata.file_type().is_file()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "Kimi bridge sync lock {} is not a regular file",
                            path.display()
                        ),
                    ));
                }
                OpenOptions::new().read(true).write(true).open(&path)?
            }
            Err(error) => return Err(error),
        };
        if crate::platform::path_leaf_is_symlink_or_reparse(&path)?
            || !file.metadata()?.file_type().is_file()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Kimi bridge sync lock {} changed while it was opened",
                    path.display()
                ),
            ));
        }
        let started = Instant::now();
        loop {
            match fs::File::try_lock(&file) {
                Ok(()) => return Ok(Self { file }),
                Err(fs::TryLockError::WouldBlock) => {
                    if started.elapsed() >= SYNC_LOCK_TIMEOUT {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "timed out serializing Kimi bridge copy-back",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(fs::TryLockError::Error(error)) => return Err(error),
            }
        }
    }
}

impl Drop for BridgeSyncLock {
    fn drop(&mut self) {
        let _ = fs::File::unlock(&self.file);
    }
}

fn bridge_key(identity: &str) -> String {
    let identity = identity.trim();
    let identity = if identity.is_empty() {
        "unscoped"
    } else {
        identity
    };
    let digest = Sha256::digest(identity.as_bytes());
    format!(
        "session-{}",
        digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn private_name(name: &OsStr) -> bool {
    PRIVATE_NAMES
        .iter()
        .any(|private| name == OsStr::new(private))
}

fn history_name(name: &OsStr) -> bool {
    HISTORY_NAMES
        .iter()
        .any(|history| name == OsStr::new(history))
}

fn ensure_private_managed_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(path)?,
        Err(error) => return Err(error),
    }
    require_real_directory(path, "Kimi managed bridge directory")?;
    set_private_dir_permissions(path)?;
    require_real_directory(path, "Kimi managed bridge directory")
}

fn require_real_directory(path: &Path, label: &str) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if crate::platform::path_leaf_is_symlink_or_reparse(path)? || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} {} is not a real directory", path.display()),
        ));
    }
    Ok(())
}

fn validate_managed_bridge_path(bridge: &Path) -> io::Result<()> {
    let bridge_parent = bridge
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Kimi bridge has no parent"))?;
    let primary_home = bridge_parent.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Kimi bridge directory has no primary home",
        )
    })?;
    if bridge_parent.file_name() != Some(OsStr::new(BRIDGE_PARENT))
        || !bridge
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("session-"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Kimi bridge is outside the managed bridge directory",
        ));
    }
    require_real_directory(bridge_parent, "Kimi managed bridge parent")?;
    require_real_directory(bridge, "Kimi managed bridge directory")?;
    let canonical_primary = fs::canonicalize(primary_home)?;
    let canonical_parent = fs::canonicalize(bridge_parent)?;
    let canonical_bridge = fs::canonicalize(bridge)?;
    if canonical_parent.parent() != Some(canonical_primary.as_path())
        || canonical_bridge.parent() != Some(canonical_parent.as_path())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Kimi bridge canonical path escaped its managed parent",
        ));
    }
    Ok(())
}

fn prune_stale_bridge_snapshots(primary_home: &Path, bridge: &Path) -> io::Result<()> {
    validate_managed_bridge_path(bridge)?;
    for entry in fs::read_dir(bridge)? {
        let entry = entry?;
        let name = entry.file_name();
        if private_name(&name) || history_name(&name) {
            continue;
        }
        match fs::symlink_metadata(primary_home.join(&name)) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                remove_entry_no_follow(&entry.path())?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn mirror_entry(source: &Path, destination: &Path) -> io::Result<()> {
    let source_metadata = fs::symlink_metadata(source)?;
    reject_non_symlink_reparse(source, &source_metadata, "Kimi primary-home entry")?;
    if destination.exists() || fs::symlink_metadata(destination).is_ok() {
        // A symlink sees future source updates. A copy fallback is refreshed
        // below so an auth/config update is not stranded in an old bridge.
        let destination_metadata = fs::symlink_metadata(destination)?;
        reject_non_symlink_reparse(destination, &destination_metadata, "Kimi bridge entry")?;
        if destination_metadata.file_type().is_symlink() {
            if same_canonical_path(source, destination) {
                return Ok(());
            }
            remove_entry_no_follow(destination)?;
        } else {
            return sync_copy(source, destination);
        }
    }

    match symlink_entry(source, destination) {
        Ok(()) => Ok(()),
        Err(_) => sync_copy(source, destination),
    }
}

#[cfg(unix)]
fn symlink_entry(source: &Path, destination: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(source, destination)
}

#[cfg(windows)]
fn symlink_entry(source: &Path, destination: &Path) -> io::Result<()> {
    let metadata = fs::metadata(source)?;
    if metadata.is_dir() {
        std::os::windows::fs::symlink_dir(source, destination)
    } else {
        std::os::windows::fs::symlink_file(source, destination)
    }
}

#[cfg(not(any(unix, windows)))]
fn symlink_entry(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symbolic links unavailable",
    ))
}

fn sync_copy(source: &Path, destination: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return remove_entry_no_follow(destination);
    }
    reject_non_symlink_reparse(source, &metadata, "Kimi copy source")?;
    let destination_metadata = match fs::symlink_metadata(destination) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(metadata) = destination_metadata.as_ref() {
        reject_non_symlink_reparse(destination, metadata, "Kimi copy destination")?;
    }
    if destination_metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
    {
        remove_entry_no_follow(destination)?;
    }
    if same_canonical_path(source, destination) {
        return Ok(());
    }
    if metadata.is_dir() {
        if destination_metadata
            .as_ref()
            .is_some_and(|metadata| !metadata.is_dir())
        {
            remove_entry_no_follow(destination)?;
        }
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            sync_copy(&entry.path(), &destination.join(entry.file_name()))?;
        }
        for entry in fs::read_dir(destination)? {
            let entry = entry?;
            let source_entry = source.join(entry.file_name());
            match fs::symlink_metadata(source_entry) {
                Ok(metadata) if !metadata.file_type().is_symlink() => {}
                Ok(_) => remove_entry_no_follow(&entry.path())?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    remove_entry_no_follow(&entry.path())?;
                }
                Err(error) => return Err(error),
            }
        }
        return Ok(());
    }

    if destination_metadata
        .as_ref()
        .is_some_and(|metadata| metadata.is_dir())
    {
        remove_entry_no_follow(destination)?;
    }
    let should_copy = match fs::metadata(destination) {
        Ok(existing) => {
            existing.len() != metadata.len() || existing.modified().ok() < metadata.modified().ok()
        }
        Err(_) => true,
    };
    if should_copy {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
    }
    Ok(())
}

fn remove_entry_no_follow(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    reject_non_symlink_reparse(path, &metadata, "Kimi managed entry")?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        fs::remove_file(path)
    } else {
        fs::remove_dir_all(path)
    }
}

fn sync_copy_back(source: &Path, destination: &Path) -> io::Result<()> {
    let source_metadata = fs::symlink_metadata(source)?;
    if source_metadata.file_type().is_symlink() {
        return Ok(());
    }
    reject_non_symlink_reparse(source, &source_metadata, "Kimi history source")?;
    let destination_metadata = match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => return Ok(()),
        Ok(metadata) => {
            reject_non_symlink_reparse(destination, &metadata, "Kimi history destination")?;
            Some(metadata)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if same_canonical_path(source, destination) {
        return Ok(());
    }

    if source_metadata.is_dir() {
        if destination_metadata
            .as_ref()
            .is_some_and(|metadata| !metadata.is_dir())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "cannot sync Kimi directory {} over file {}",
                    source.display(),
                    destination.display()
                ),
            ));
        }
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            sync_copy_back(&entry.path(), &destination.join(entry.file_name()))?;
        }
        return Ok(());
    }

    if destination_metadata
        .as_ref()
        .is_some_and(|metadata| metadata.is_dir())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "cannot sync Kimi file {} over directory {}",
                source.display(),
                destination.display()
            ),
        ));
    }
    match source.file_name().and_then(OsStr::to_str) {
        Some(name) if name.ends_with(".jsonl") => {
            return merge_append_only_jsonl(source, destination);
        }
        Some("state.json") if is_kimi_session_state(source) => {
            return merge_state_json(
                source,
                destination,
                &source_metadata,
                destination_metadata.as_ref(),
            );
        }
        _ => {}
    }
    let should_copy = match destination_metadata {
        None => true,
        Some(destination_metadata) => {
            let source_modified = source_metadata.modified().ok();
            let destination_modified = destination_metadata.modified().ok();
            source_modified > destination_modified
                || (source_modified == destination_modified
                    && source_metadata.len() != destination_metadata.len())
        }
    };
    if should_copy {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
    }
    Ok(())
}

struct SessionIndexRecord {
    session_id: String,
    value: Value,
}

struct SessionIndexJournal {
    snapshot: JournalPrefixSnapshot,
    records: Vec<SessionIndexRecord>,
}

/// Merge Kimi's session index according to the reader Kimi actually ships.
///
/// `session_index.jsonl` is not a globally ordered replay journal. Kimi reduces
/// it into a map keyed by `sessionId`; later records for one id replace or
/// delete only that id. Concurrent bridges therefore legitimately produce
/// `[parent, child-a]` and `[parent, child-b]`. Treating those as an ordinary
/// append-only journal rejects the second branch, while concatenating arbitrary
/// transcript journals would be unsafe.
///
/// Compare each session's own record sequence and append only per-session
/// suffixes. Distinct ids commute. A conflicting sequence for the same id still
/// fails closed because there is no timestamp with which to order it. Active
/// entries are rebased to the destination bridge's `sessions/` path; Kimi
/// validates that absolute path lexically when it reads the index, so sharing a
/// symlinked index between bridge homes makes otherwise valid sessions vanish.
fn merge_session_index(
    source: &Path,
    destination: &Path,
    destination_home: &Path,
) -> io::Result<()> {
    if same_canonical_path(source, destination) {
        return Ok(());
    }
    if crate::platform::path_leaf_is_symlink_or_reparse(source)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Kimi session index {} is a link", source.display()),
        ));
    }
    if !destination_home.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Kimi session-index destination home must be absolute",
        ));
    }
    let source_journal = read_session_index_journal(source, destination_home, true)?;
    let destination_journal = match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            reject_non_symlink_reparse(destination, &metadata, "Kimi session index")?;
            if metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Kimi session index {} is a link", destination.display()),
                ));
            }
            Some(read_session_index_journal(
                destination,
                destination_home,
                false,
            )?)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };

    let mut source_by_id = std::collections::HashMap::<&str, Vec<&Value>>::new();
    for record in &source_journal.records {
        source_by_id
            .entry(record.session_id.as_str())
            .or_default()
            .push(&record.value);
    }
    let mut destination_by_id = std::collections::HashMap::<&str, Vec<&Value>>::new();
    if let Some(journal) = destination_journal.as_ref() {
        for record in &journal.records {
            destination_by_id
                .entry(record.session_id.as_str())
                .or_default()
                .push(&record.value);
        }
    }

    let mut destination_counts = std::collections::HashMap::<&str, usize>::new();
    for (session_id, source_records) in &source_by_id {
        let destination_records = destination_by_id
            .get(session_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let common = source_records.len().min(destination_records.len());
        if source_records[..common] != destination_records[..common] {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("refusing to merge divergent Kimi session-index history for {session_id}"),
            ));
        }
        destination_counts.insert(session_id, destination_records.len());
    }

    let mut seen = std::collections::HashMap::<&str, usize>::new();
    let mut append = Vec::new();
    for record in &source_journal.records {
        let occurrence = seen.entry(record.session_id.as_str()).or_default();
        let destination_count = destination_counts
            .get(record.session_id.as_str())
            .copied()
            .unwrap_or_default();
        if *occurrence >= destination_count {
            serde_json::to_writer(&mut append, &record.value).map_err(io::Error::other)?;
            append.push(b'\n');
        }
        *occurrence = occurrence.saturating_add(1);
    }
    if append.is_empty() {
        return Ok(());
    }

    // Revalidate the exact source prefix that produced `append`, then validate
    // the destination immediately before opening it for append. Source growth
    // is safe and will be picked up by the next sweep; replacement, truncation,
    // or an in-place rewrite is not.
    let _validated_source =
        open_validated_journal_prefix(source, &source_journal.snapshot, true, false)?;
    let destination_snapshot = destination_journal
        .as_ref()
        .map(|journal| &journal.snapshot);
    let mut output = open_validated_journal_destination(destination, destination_snapshot)?;
    if destination_snapshot.is_none() {
        set_private_file_permissions(destination)?;
    }
    output.write_all(&append)?;
    output.sync_data()
}

fn read_session_index_journal(
    path: &Path,
    destination_home: &Path,
    allow_incomplete_tail: bool,
) -> io::Result<SessionIndexJournal> {
    let file = fs::File::open(path)?;
    let identity = reliable_file_identity(&file, path)?;
    let mut reader = BufReader::new(file);
    let mut digest = Sha256::new();
    let mut complete_len = 0u64;
    let mut records = Vec::new();
    while let Some(encoded) = read_complete_journal_record(&mut reader)? {
        if !encoded.ends_with(b"\n") {
            if allow_incomplete_tail {
                break;
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "refusing to append Kimi session index {} after an incomplete record",
                    path.display()
                ),
            ));
        }
        complete_len = checked_journal_len(complete_len, encoded.len())?;
        digest.update(&encoded);
        records.push(normalize_session_index_record(
            &encoded,
            path,
            destination_home,
        )?);
    }
    if !allow_incomplete_tail && reader.get_ref().metadata()?.len() != complete_len {
        return Err(journal_changed_error(path, "while it was compared"));
    }
    Ok(SessionIndexJournal {
        snapshot: JournalPrefixSnapshot {
            identity,
            len: complete_len,
            digest: digest.finalize().into(),
        },
        records,
    })
}

fn normalize_session_index_record(
    encoded: &[u8],
    source: &Path,
    destination_home: &Path,
) -> io::Result<SessionIndexRecord> {
    let mut value = serde_json::from_slice::<Value>(encoded).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Kimi session index {}: {error}", source.display()),
        )
    })?;
    let object = value.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Kimi session index {} contains a non-object record",
                source.display()
            ),
        )
    })?;
    let session_id = object
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|session_id| {
            !session_id.is_empty()
                && *session_id != "."
                && *session_id != ".."
                && !session_id.contains('/')
                && !session_id.contains('\\')
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Kimi session index {} contains an invalid sessionId",
                    source.display()
                ),
            )
        })?
        .to_string();
    if object.get("deleted").and_then(Value::as_bool) != Some(true) {
        if object.get("workDir").and_then(Value::as_str).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Kimi session index {} contains an active record without workDir",
                    source.display()
                ),
            ));
        }
        let session_dir = object
            .get("sessionDir")
            .and_then(Value::as_str)
            .map(Path::new)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Kimi session index {} contains an invalid sessionDir",
                        source.display()
                    ),
                )
            })?;
        if session_dir.file_name() != Some(OsStr::new(&session_id)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Kimi session index {} sessionDir does not match sessionId",
                    source.display()
                ),
            ));
        }
        let workspace = session_dir
            .parent()
            .and_then(Path::file_name)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Kimi session index {} sessionDir has no workspace",
                        source.display()
                    ),
                )
            })?
            .to_os_string();
        if session_dir
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            != Some(OsStr::new("sessions"))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "Kimi session index {} sessionDir is outside a sessions directory",
                    source.display()
                ),
            ));
        }
        let rebased = destination_home
            .join("sessions")
            .join(workspace)
            .join(&session_id);
        let rebased = rebased.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Kimi session-index destination is not valid Unicode",
            )
        })?;
        object.insert("sessionDir".into(), Value::String(rebased.to_string()));
    }
    Ok(SessionIndexRecord { session_id, value })
}

/// Ordinary symlinks are handled explicitly by the caller: primary-backed
/// bridge entries are expected to be symlinks on platforms that support them.
/// A Windows junction or other non-symlink reparse point must never fall
/// through as an ordinary file/directory, because recursive copy and cleanup
/// operations would otherwise traverse outside the managed bridge.
fn reject_non_symlink_reparse(path: &Path, metadata: &fs::Metadata, label: &str) -> io::Result<()> {
    if !metadata.file_type().is_symlink() && crate::platform::path_leaf_is_symlink_or_reparse(path)?
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} {} is a reparse point", path.display()),
        ));
    }
    Ok(())
}

fn is_kimi_session_state(path: &Path) -> bool {
    path.file_name() == Some(OsStr::new("state.json"))
        && (path
            .ancestors()
            .nth(3)
            .and_then(Path::file_name)
            .is_some_and(|name| name == OsStr::new("sessions"))
            || path
                .parent()
                .is_some_and(|directory| directory.join("agents").is_dir()))
}

/// Merge Kimi's append-only journals without inventing an order for divergent
/// histories.
///
/// A journal can safely advance only when either side is an exact ordered
/// prefix of the other. If the primary is a prefix, append the bridge's suffix;
/// if the bridge is a prefix, the primary is already newer and remains
/// untouched. Any mismatch fails closed: treating the records as an unordered
/// set would turn primary `[A, C]` plus bridge `[A, B, C]` into `[A, C, B]`,
/// corrupting the chronology Kimi uses for replay and fork/undo.
///
/// Records are compared byte-for-byte, so legitimate duplicates retain their
/// multiplicity and position. A partial source tail is ignored until Kimi
/// finishes it; a partial primary tail fails closed rather than concatenating
/// two records.
fn merge_append_only_jsonl(source: &Path, destination: &Path) -> io::Result<()> {
    merge_append_only_jsonl_with_before_append(source, destination, || {})
}

fn merge_append_only_jsonl_with_before_append(
    source: &Path,
    destination: &Path,
    before_append: impl FnOnce(),
) -> io::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }

    let source_file = fs::File::open(source)?;
    let source_identity = reliable_file_identity(&source_file, source)?;
    let mut source_reader = BufReader::new(source_file);
    let mut source_digest = Sha256::new();
    let mut source_complete_len = 0u64;
    let destination_open = match fs::File::open(destination) {
        Ok(file) => {
            let identity = reliable_file_identity(&file, destination)?;
            Some((BufReader::new(file), identity))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let (mut destination_reader, destination_identity) = match destination_open {
        Some((reader, identity)) => (Some(reader), Some(identity)),
        None => (None, None),
    };
    let mut destination_digest = Sha256::new();
    let mut destination_complete_len = 0u64;
    let mut compared_records = 0usize;

    if let Some(reader) = destination_reader.as_mut() {
        while let Some(destination_record) = read_complete_journal_record(reader)? {
            if !destination_record.ends_with(b"\n") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "refusing to append Kimi journal {} after an incomplete record",
                        destination.display()
                    ),
                ));
            }
            destination_complete_len =
                checked_journal_len(destination_complete_len, destination_record.len())?;
            destination_digest.update(&destination_record);

            let Some(source_record) = read_complete_journal_record(&mut source_reader)? else {
                // The bridge is an exact prefix of the primary (or has only an
                // unfinished next record), so the primary is already ahead.
                return Ok(());
            };
            if !source_record.ends_with(b"\n") {
                // A source partial tail is not durable yet. The complete
                // source records compared so far are a prefix of the primary.
                return Ok(());
            }
            source_complete_len = checked_journal_len(source_complete_len, source_record.len())?;
            source_digest.update(&source_record);
            compared_records = compared_records.saturating_add(1);
            if source_record != destination_record {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "refusing to merge divergent Kimi journals {} and {} at record {}",
                        source.display(),
                        destination.display(),
                        compared_records
                    ),
                ));
            }
        }
        if reader.get_ref().metadata()?.len() != destination_complete_len {
            return Err(journal_changed_error(destination, "while it was compared"));
        }
    }

    while let Some(record) = read_complete_journal_record(&mut source_reader)? {
        if !record.ends_with(b"\n") {
            break;
        }
        source_complete_len = checked_journal_len(source_complete_len, record.len())?;
        source_digest.update(&record);
    }
    if source_complete_len == destination_complete_len {
        return Ok(());
    }

    let source_snapshot = JournalPrefixSnapshot {
        identity: source_identity,
        len: source_complete_len,
        digest: source_digest.finalize().into(),
    };
    let destination_snapshot = destination_identity.map(|identity| JournalPrefixSnapshot {
        identity,
        len: destination_complete_len,
        digest: destination_digest.finalize().into(),
    });

    // Tests use this seam to deterministically exercise a primary writer that
    // races the comparison. Production passes a no-op closure.
    before_append();

    // Re-open and hash both exact prefixes after comparison. Identity catches
    // replacement; length catches a primary append; the digest catches an
    // in-place same-length rewrite. Source growth is safe—the already-read
    // complete prefix remains ordered—but replacement/truncation/rewrite is
    // not. Validate the destination last, immediately before the append.
    let mut validated_source =
        open_validated_journal_prefix(source, &source_snapshot, true, false)?;
    validated_source.seek(SeekFrom::Start(destination_complete_len))?;

    let suffix_len = source_complete_len
        .checked_sub(destination_complete_len)
        .ok_or_else(|| io::Error::other("Kimi journal suffix length underflow"))?;
    // Stage the validated suffix before opening the primary for append. If the
    // source is truncated while being copied, the primary remains untouched
    // instead of receiving a partial JSON record. Keep the temporary beside
    // the private bridge history (rather than in a system-wide temp directory)
    // and stamp it owner-private because it contains conversation text.
    let staging_directory = source.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Kimi journal source has no staging directory",
        )
    })?;
    let mut staged_suffix = tempfile::NamedTempFile::new_in(staging_directory)?;
    set_private_file_permissions(staged_suffix.path())?;
    let copied = io::copy(
        &mut validated_source.take(suffix_len),
        staged_suffix.as_file_mut(),
    )?;
    if copied != suffix_len {
        return Err(journal_changed_error(source, "while its suffix was copied"));
    }
    staged_suffix.as_file_mut().seek(SeekFrom::Start(0))?;

    let mut output =
        open_validated_journal_destination(destination, destination_snapshot.as_ref())?;
    let copied = io::copy(
        &mut staged_suffix.as_file_mut().take(suffix_len),
        &mut output,
    )?;
    if copied != suffix_len {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            format!(
                "failed to append the complete Kimi journal suffix to {}",
                destination.display()
            ),
        ));
    }
    output.sync_data()
}

fn read_complete_journal_record(reader: &mut BufReader<fs::File>) -> io::Result<Option<Vec<u8>>> {
    let mut record = Vec::new();
    let read = reader.read_until(b'\n', &mut record)?;
    Ok((read != 0).then_some(record))
}

#[derive(Clone, Copy)]
struct JournalPrefixSnapshot {
    identity: crate::platform::FileIdentity,
    len: u64,
    digest: [u8; 32],
}

fn checked_journal_len(current: u64, record_len: usize) -> io::Result<u64> {
    current
        .checked_add(record_len as u64)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Kimi journal length overflow"))
}

fn reliable_file_identity(
    file: &fs::File,
    path: &Path,
) -> io::Result<crate::platform::FileIdentity> {
    let identity = crate::platform::FileIdentity::from_file(file)?;
    if !identity.is_reliable() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Kimi journal {} has no reliable file identity",
                path.display()
            ),
        ));
    }
    Ok(identity)
}

fn open_validated_journal_prefix(
    path: &Path,
    snapshot: &JournalPrefixSnapshot,
    allow_growth: bool,
    append: bool,
) -> io::Result<fs::File> {
    if crate::platform::path_leaf_is_symlink_or_reparse(path)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Kimi journal {} became a link", path.display()),
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).append(append);
    let mut file = options.open(path)?;
    if crate::platform::path_leaf_is_symlink_or_reparse(path)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Kimi journal {} became a link while opening",
                path.display()
            ),
        ));
    }
    let identity = reliable_file_identity(&file, path)?;
    if identity != snapshot.identity {
        return Err(journal_changed_error(path, "was replaced before append"));
    }
    let len = file.metadata()?.len();
    if (allow_growth && len < snapshot.len) || (!allow_growth && len != snapshot.len) {
        return Err(journal_changed_error(path, "changed length before append"));
    }
    if hash_file_prefix(&mut file, snapshot.len)? != snapshot.digest {
        return Err(journal_changed_error(path, "changed content before append"));
    }
    let path_identity = crate::platform::FileIdentity::from_path(path)?;
    if !path_identity.is_reliable() || path_identity != identity {
        return Err(journal_changed_error(path, "was replaced while opening"));
    }
    if !allow_growth && file.metadata()?.len() != snapshot.len {
        return Err(journal_changed_error(
            path,
            "changed length during validation",
        ));
    }
    Ok(file)
}

fn open_validated_journal_destination(
    path: &Path,
    snapshot: Option<&JournalPrefixSnapshot>,
) -> io::Result<fs::File> {
    match snapshot {
        Some(snapshot) => open_validated_journal_prefix(path, snapshot, false, true),
        None => {
            if fs::symlink_metadata(path).is_ok() {
                return Err(journal_changed_error(path, "appeared before append"));
            }
            let mut file = OpenOptions::new()
                .read(true)
                .append(true)
                .create_new(true)
                .open(path)?;
            if crate::platform::path_leaf_is_symlink_or_reparse(path)?
                || file.metadata()?.len() != 0
            {
                return Err(journal_changed_error(path, "changed while it was created"));
            }
            let opened_identity = reliable_file_identity(&file, path)?;
            let path_identity = crate::platform::FileIdentity::from_path(path)?;
            if !path_identity.is_reliable() || path_identity != opened_identity {
                return Err(journal_changed_error(path, "was replaced while opening"));
            }
            file.seek(SeekFrom::Start(0))?;
            Ok(file)
        }
    }
}

fn hash_file_prefix(file: &mut fs::File, len: u64) -> io::Result<[u8; 32]> {
    file.seek(SeekFrom::Start(0))?;
    let mut remaining = len;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let requested = remaining.min(buffer.len() as u64) as usize;
        let read = file.read(&mut buffer[..requested])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Kimi journal changed while its prefix was validated",
            ));
        }
        digest.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(digest.finalize().into())
}

fn journal_changed_error(path: &Path, detail: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "refusing to append Kimi journal {} because it {detail}",
            path.display()
        ),
    )
}

/// Kimi session state is a JSON object which may gain agent-specific keys in
/// different bridge copies. Recursively union object members and let the newer
/// file win scalar/array conflicts. This preserves independently spawned
/// agents while keeping the latest lifecycle value for shared fields.
fn merge_state_json(
    source: &Path,
    destination: &Path,
    source_metadata: &fs::Metadata,
    destination_metadata: Option<&fs::Metadata>,
) -> io::Result<()> {
    let source_value = read_json_object(source)?;
    if destination_metadata.is_none() {
        let encoded = serde_json::to_vec(&source_value).map_err(io::Error::other)?;
        return crate::file_watcher::atomic_write(destination, &encoded);
    }

    let destination_value = read_json_object(destination)?;
    let source_is_newer = source_metadata.modified().ok()
        >= destination_metadata.and_then(|metadata| metadata.modified().ok());
    let merged = merge_json_objects(destination_value, source_value, source_is_newer);
    let encoded = serde_json::to_vec(&merged).map_err(io::Error::other)?;
    crate::file_watcher::atomic_write(destination, &encoded)
}

fn read_json_object(path: &Path) -> io::Result<Value> {
    let value = serde_json::from_slice::<Value>(&fs::read(path)?).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Kimi state {}: {error}", path.display()),
        )
    })?;
    if !value.is_object() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Kimi state {} must contain a JSON object", path.display()),
        ));
    }
    Ok(value)
}

fn merge_json_objects(destination: Value, source: Value, source_is_newer: bool) -> Value {
    match (destination, source) {
        (Value::Object(mut destination), Value::Object(source)) => {
            for (key, source_value) in source {
                match destination.remove(&key) {
                    Some(destination_value) => {
                        destination.insert(
                            key,
                            merge_json_objects(destination_value, source_value, source_is_newer),
                        );
                    }
                    None => {
                        destination.insert(key, source_value);
                    }
                }
            }
            Value::Object(destination)
        }
        (destination, source) => {
            if source_is_newer {
                source
            } else {
                destination
            }
        }
    }
}

fn same_canonical_path(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn write_merged_mcp(
    primary_home: &Path,
    bridge: &Path,
    mcp: Option<&BridgeMcpConfig>,
) -> io::Result<()> {
    validate_managed_bridge_path(bridge)?;
    let primary_path = primary_home.join("mcp.json");
    let mut root = match fs::read_to_string(&primary_path) {
        Ok(text) => serde_json::from_str::<Value>(&text).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid {}: {error}", primary_path.display()),
            )
        })?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(error) => return Err(error),
    };
    let root_object = root.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} must contain a JSON object", primary_path.display()),
        )
    })?;
    let servers = root_object
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} field mcpServers must contain a JSON object",
                    primary_path.display()
                ),
            )
        })?;

    if let Some(mcp) = mcp {
        let mut intendant = serde_json::json!({
            "transport": "http",
            "url": mcp.url,
            "enabled": true,
        });
        if let Some(name) = mcp
            .bearer_token_env_var
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            intendant["bearerTokenEnvVar"] = Value::String(name.to_string());
        }
        // The supervisor-owned capability must win over a same-named user
        // declaration. Other user MCP entries remain byte-semantically intact.
        servers.insert(mcp.server_name.clone(), intendant);
    }

    let encoded = serde_json::to_vec_pretty(&root).map_err(io::Error::other)?;
    let target = bridge.join("mcp.json");
    let mut temporary = tempfile::Builder::new()
        .prefix(".mcp.json.tmp-")
        .tempfile_in(bridge)?;
    temporary.as_file_mut().write_all(&encoded)?;
    temporary.as_file_mut().flush()?;
    set_private_file_permissions(temporary.path())?;
    #[cfg(windows)]
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Kimi MCP target {} is a directory", target.display()),
            ));
        }
        Ok(_) => remove_entry_no_follow(&target)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    temporary.persist(&target).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(unix)]
fn set_private_dir_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(windows)]
fn set_private_dir_permissions(path: &Path) -> io::Result<()> {
    crate::platform::set_owner_private_permissions(path)
}

#[cfg(not(any(unix, windows)))]
fn set_private_dir_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    crate::platform::set_owner_private_permissions(path)
}

#[cfg(not(any(unix, windows)))]
fn set_private_file_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn session_index_entry(
        home: &Path,
        workspace: &str,
        session_id: &str,
        work_dir: &Path,
    ) -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "sessionId": session_id,
                "sessionDir": home
                    .join("sessions")
                    .join(workspace)
                    .join(session_id)
                    .to_string_lossy(),
                "workDir": work_dir.to_string_lossy(),
            })
        )
    }

    fn read_session_index(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn bridge_key_is_stable_and_path_safe() {
        let a = bridge_key("session/with spaces");
        let b = bridge_key("session/with spaces");
        assert_eq!(a, b);
        assert!(a.starts_with("session-"));
        assert_eq!(a.len(), "session-".len() + 24);
        assert!(a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    }

    #[test]
    fn bridge_merges_mcp_without_storing_a_token() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("kimi");
        fs::create_dir_all(&primary).unwrap();
        fs::write(
            primary.join("mcp.json"),
            r#"{"mcpServers":{"existing":{"command":"example"},"intendant":{"url":"bad"}}}"#,
        )
        .unwrap();
        fs::write(primary.join("config.toml"), "model = \"test\"\n").unwrap();

        let bridge = prepare_bridge_home(
            &primary,
            "intendant-session",
            Some(&BridgeMcpConfig {
                server_name: "intendant".into(),
                url: "http://localhost:8765/mcp?session_id=x".into(),
                bearer_token_env_var: Some("INTENDANT_MCP_BEARER_TOKEN".into()),
            }),
        )
        .unwrap();
        let value: Value =
            serde_json::from_str(&fs::read_to_string(bridge.join("mcp.json")).unwrap()).unwrap();
        assert_eq!(
            value["mcpServers"]["existing"]["command"],
            Value::String("example".into())
        );
        assert_eq!(
            value["mcpServers"]["intendant"]["bearerTokenEnvVar"],
            Value::String("INTENDANT_MCP_BEARER_TOKEN".into())
        );
        let encoded = value.to_string();
        assert!(!encoded.contains("secret"));
        assert!(bridge.join("config.toml").exists());
        assert!(!bridge.join("server.token").exists());
    }

    #[test]
    fn refresh_prunes_stale_authority_snapshots_but_preserves_history() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("kimi");
        let identity = "stable-session";
        let bridge = primary.join(BRIDGE_PARENT).join(bridge_key(identity));
        fs::create_dir_all(bridge.join("credentials")).unwrap();
        fs::write(bridge.join("credentials/kimi-code.json"), "stale-authority").unwrap();
        fs::write(bridge.join("config.toml"), "stale-config").unwrap();
        fs::create_dir_all(bridge.join("sessions/wd/session")).unwrap();
        fs::write(
            bridge.join("sessions/wd/session/wire.jsonl"),
            "{\"type\":\"turn.ended\"}\n",
        )
        .unwrap();

        // The primary credentials directory still exists, but logout removed
        // the authority-bearing file. The top-level config disappeared
        // entirely. Both stale copy-fallback shapes must be reconciled.
        fs::create_dir_all(primary.join("credentials")).unwrap();
        fs::write(primary.join("credentials/current-marker"), "current").unwrap();

        let refreshed = prepare_bridge_home(&primary, identity, None).unwrap();

        assert_eq!(refreshed, bridge);
        assert!(!bridge.join("credentials/kimi-code.json").exists());
        assert_eq!(
            fs::read_to_string(bridge.join("credentials/current-marker")).unwrap(),
            "current"
        );
        assert!(!bridge.join("config.toml").exists());
        assert_eq!(
            fs::read_to_string(bridge.join("sessions/wd/session/wire.jsonl")).unwrap(),
            "{\"type\":\"turn.ended\"}\n"
        );
    }

    #[test]
    fn new_bridge_sees_history_written_by_a_live_copy_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("kimi");
        let parent = prepare_bridge_home(&primary, "parent-wrapper", None).unwrap();
        let parent_session = parent.join("sessions/wd/session_parent");
        fs::create_dir_all(&parent_session).unwrap();
        fs::write(
            parent_session.join("wire.jsonl"),
            "{\"type\":\"turn.ended\"}\n",
        )
        .unwrap();
        fs::write(
            parent.join("session_index.jsonl"),
            session_index_entry(&parent, "wd", "session_parent", temp.path()),
        )
        .unwrap();

        let child = prepare_bridge_home(&primary, "child-wrapper", None).unwrap();

        assert_ne!(child, parent);
        assert_eq!(
            fs::read_to_string(child.join("sessions/wd/session_parent/wire.jsonl")).unwrap(),
            "{\"type\":\"turn.ended\"}\n"
        );
        let index = read_session_index(&child.join("session_index.jsonl"));
        assert_eq!(index.len(), 1);
        assert_eq!(index[0]["sessionId"], "session_parent");
        assert_eq!(
            index[0]["sessionDir"],
            child
                .join("sessions/wd/session_parent")
                .to_string_lossy()
                .as_ref()
        );
    }

    #[test]
    fn new_bridge_merges_distinct_children_from_divergent_live_indexes() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("kimi");
        let parent = prepare_bridge_home(&primary, "parent-wrapper", None).unwrap();
        fs::create_dir_all(parent.join("sessions/wd/session_parent")).unwrap();
        fs::write(
            parent.join("sessions/wd/session_parent/wire.jsonl"),
            "{\"type\":\"parent\"}\n",
        )
        .unwrap();
        fs::write(
            parent.join(SESSION_INDEX_NAME),
            session_index_entry(&parent, "wd", "session_parent", temp.path()),
        )
        .unwrap();

        let child_a = prepare_bridge_home(&primary, "child-a-wrapper", None).unwrap();
        fs::create_dir_all(child_a.join("sessions/wd/session_child_a")).unwrap();
        fs::write(
            child_a.join("sessions/wd/session_child_a/wire.jsonl"),
            "{\"type\":\"child-a\"}\n",
        )
        .unwrap();
        OpenOptions::new()
            .append(true)
            .open(child_a.join(SESSION_INDEX_NAME))
            .unwrap()
            .write_all(
                session_index_entry(&child_a, "wd", "session_child_a", temp.path()).as_bytes(),
            )
            .unwrap();

        fs::create_dir_all(parent.join("sessions/wd/session_child_b")).unwrap();
        fs::write(
            parent.join("sessions/wd/session_child_b/wire.jsonl"),
            "{\"type\":\"child-b\"}\n",
        )
        .unwrap();
        OpenOptions::new()
            .append(true)
            .open(parent.join(SESSION_INDEX_NAME))
            .unwrap()
            .write_all(
                session_index_entry(&parent, "wd", "session_child_b", temp.path()).as_bytes(),
            )
            .unwrap();

        let consumer = prepare_bridge_home(&primary, "consumer-wrapper", None).unwrap();
        let index = read_session_index(&consumer.join(SESSION_INDEX_NAME));
        let ids = index
            .iter()
            .map(|record| record["sessionId"].as_str().unwrap())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            ids,
            std::collections::HashSet::from([
                "session_parent",
                "session_child_a",
                "session_child_b",
            ])
        );
        assert!(index.iter().all(|record| record["sessionDir"]
            .as_str()
            .unwrap()
            .starts_with(consumer.join("sessions").to_string_lossy().as_ref())));
        assert_eq!(
            fs::read_to_string(consumer.join("sessions/wd/session_child_a/wire.jsonl")).unwrap(),
            "{\"type\":\"child-a\"}\n"
        );
        assert_eq!(
            fs::read_to_string(consumer.join("sessions/wd/session_child_b/wire.jsonl")).unwrap(),
            "{\"type\":\"child-b\"}\n"
        );
    }

    #[test]
    fn session_index_merge_preserves_per_session_order_and_rejects_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let source_home = temp.path().join("source-home");
        let destination_home = temp.path().join("destination-home");
        fs::create_dir_all(&source_home).unwrap();
        fs::create_dir_all(&destination_home).unwrap();
        let source = source_home.join(SESSION_INDEX_NAME);
        let destination = destination_home.join(SESSION_INDEX_NAME);
        let active = session_index_entry(&source_home, "wd", "session-a", temp.path());
        fs::write(
            &destination,
            session_index_entry(&destination_home, "wd", "session-a", temp.path()),
        )
        .unwrap();
        fs::write(
            &source,
            format!(
                "{active}{}\n",
                serde_json::json!({"sessionId": "session-a", "deleted": true})
            ),
        )
        .unwrap();

        merge_session_index(&source, &destination, &destination_home).unwrap();
        let records = read_session_index(&destination);
        assert_eq!(records.len(), 2);
        assert_eq!(records[1]["deleted"], true);

        fs::write(
            &source,
            session_index_entry(
                &source_home,
                "wd",
                "session-a",
                &temp.path().join("different-workdir"),
            ),
        )
        .unwrap();
        let error = merge_session_index(&source, &destination, &destination_home).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(read_session_index(&destination), records);
    }

    #[test]
    fn malformed_primary_mcp_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("kimi");
        fs::create_dir_all(&primary).unwrap();
        fs::write(primary.join("mcp.json"), "[]").unwrap();
        let error = prepare_bridge_home(&primary, "session", None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn project_collision_gets_a_stable_non_shadowed_name() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join(".git")).unwrap();
        fs::write(
            temp.path().join(".mcp.json"),
            r#"{"mcpServers":{"intendant":{"url":"http://project.invalid"},"intendant_managed_01234567":{"url":"http://also.invalid"}}}"#,
        )
        .unwrap();
        let first = choose_mcp_server_name(temp.path(), "session-a").unwrap();
        let second = choose_mcp_server_name(temp.path(), "session-b").unwrap();
        assert_eq!(first, second);
        assert_ne!(first, "intendant");
        assert!(first.starts_with("intendant_managed_"));
    }

    fn credential_copy_pair(temp: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        let primary = temp.path().join("kimi");
        let bridge = primary.join(BRIDGE_PARENT).join("session-test");
        fs::create_dir_all(primary.join("credentials")).unwrap();
        fs::create_dir_all(bridge.join("credentials")).unwrap();
        fs::write(primary.join(KIMI_CREDENTIAL_PATH), b"synthetic-initial").unwrap();
        fs::write(bridge.join(KIMI_CREDENTIAL_PATH), b"synthetic-initial").unwrap();
        (primary, bridge)
    }

    #[test]
    fn copy_fallback_adopts_rotated_oauth_credential_by_cas() {
        let temp = tempfile::tempdir().unwrap();
        let (primary, bridge) = credential_copy_pair(&temp);
        let mut mirror = prepare_credential_refresh_mirror(&primary, &bridge)
            .unwrap()
            .expect("real credential copy needs a refresh mirror");

        fs::write(bridge.join(KIMI_CREDENTIAL_PATH), b"synthetic-rotated").unwrap();
        assert_eq!(mirror.sync_once().unwrap(), CredentialRefreshSync::Updated);
        assert_eq!(
            fs::read(primary.join(KIMI_CREDENTIAL_PATH)).unwrap(),
            b"synthetic-rotated"
        );
        assert_eq!(
            mirror.sync_once().unwrap(),
            CredentialRefreshSync::Unchanged
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(primary.join(KIMI_CREDENTIAL_PATH))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o077,
                0
            );
        }
    }

    #[test]
    fn copy_fallback_never_overwrites_concurrent_login_or_refresh() {
        let temp = tempfile::tempdir().unwrap();
        let (primary, bridge) = credential_copy_pair(&temp);
        let mut mirror = prepare_credential_refresh_mirror(&primary, &bridge)
            .unwrap()
            .expect("real credential copy needs a refresh mirror");

        fs::write(bridge.join(KIMI_CREDENTIAL_PATH), b"synthetic-rotated").unwrap();
        fs::write(primary.join(KIMI_CREDENTIAL_PATH), b"synthetic-concurrent").unwrap();
        assert_eq!(
            mirror.sync_once().unwrap(),
            CredentialRefreshSync::SourceChanged
        );
        assert_eq!(
            fs::read(primary.join(KIMI_CREDENTIAL_PATH)).unwrap(),
            b"synthetic-concurrent"
        );

        // Detachment is sticky: even if the source later happens to match the
        // old bytes again, stale bridge authority cannot be replayed.
        fs::write(primary.join(KIMI_CREDENTIAL_PATH), b"synthetic-initial").unwrap();
        assert_eq!(
            mirror.sync_once().unwrap(),
            CredentialRefreshSync::SourceChanged
        );
        assert_eq!(
            fs::read(primary.join(KIMI_CREDENTIAL_PATH)).unwrap(),
            b"synthetic-initial"
        );
    }

    #[test]
    fn copy_fallback_never_resurrects_logged_out_credential() {
        let temp = tempfile::tempdir().unwrap();
        let (primary, bridge) = credential_copy_pair(&temp);
        let mut mirror = prepare_credential_refresh_mirror(&primary, &bridge)
            .unwrap()
            .expect("real credential copy needs a refresh mirror");

        fs::write(bridge.join(KIMI_CREDENTIAL_PATH), b"synthetic-rotated").unwrap();
        fs::remove_file(primary.join(KIMI_CREDENTIAL_PATH)).unwrap();
        assert_eq!(
            mirror.sync_once().unwrap(),
            CredentialRefreshSync::SourceChanged
        );
        assert!(!primary.join(KIMI_CREDENTIAL_PATH).exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_backed_credential_needs_no_refresh_monitor() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("kimi");
        fs::create_dir_all(primary.join("credentials")).unwrap();
        fs::write(primary.join(KIMI_CREDENTIAL_PATH), b"synthetic-initial").unwrap();
        let bridge = prepare_bridge_home(&primary, "session", None).unwrap();

        assert!(
            prepare_credential_refresh_mirror(&primary, &bridge)
                .unwrap()
                .is_none(),
            "the bridge and primary resolve to the same live credential"
        );
    }

    #[test]
    fn copy_fallback_sync_persists_only_history_and_never_resurrects_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("kimi");
        let bridge = primary.join(BRIDGE_PARENT).join("session-test");
        fs::create_dir_all(bridge.join("sessions/workspace/session")).unwrap();
        fs::write(
            bridge.join("sessions/workspace/session/wire.jsonl"),
            "{\"type\":\"turn.ended\"}\n",
        )
        .unwrap();
        fs::write(
            bridge.join("session_index.jsonl"),
            session_index_entry(&bridge, "workspace", "session", temp.path()),
        )
        .unwrap();
        fs::write(bridge.join("server.token"), "bridge-secret").unwrap();
        fs::create_dir_all(bridge.join("server")).unwrap();
        fs::write(bridge.join("server/lock"), "private").unwrap();
        fs::write(bridge.join("mcp.json"), "{\"mcpServers\":{}}").unwrap();
        fs::write(bridge.join("credentials.json"), "stale-authority").unwrap();
        fs::write(bridge.join("config.toml"), "model = \"stale\"\n").unwrap();
        fs::create_dir_all(bridge.join("plugins/stale")).unwrap();
        fs::write(bridge.join("plugins/stale/plugin.json"), "{}").unwrap();
        fs::create_dir_all(bridge.join("cache")).unwrap();
        fs::write(bridge.join("cache/stale"), "cache").unwrap();
        fs::create_dir_all(&primary).unwrap();
        fs::write(primary.join("config.toml"), "model = \"current\"\n").unwrap();

        sync_bridge_home_to_primary(&bridge).unwrap();

        assert_eq!(
            fs::read_to_string(primary.join("sessions/workspace/session/wire.jsonl")).unwrap(),
            "{\"type\":\"turn.ended\"}\n"
        );
        let index = read_session_index(&primary.join("session_index.jsonl"));
        assert_eq!(index.len(), 1);
        assert_eq!(index[0]["sessionId"], "session");
        assert_eq!(
            index[0]["sessionDir"],
            primary
                .join("sessions/workspace/session")
                .to_string_lossy()
                .as_ref()
        );
        assert!(!primary.join("server.token").exists());
        assert!(!primary.join("server").exists());
        assert!(!primary.join("mcp.json").exists());
        assert!(!primary.join("credentials.json").exists());
        assert_eq!(
            fs::read_to_string(primary.join("config.toml")).unwrap(),
            "model = \"current\"\n"
        );
        assert!(!primary.join("plugins").exists());
        assert!(!primary.join("cache").exists());
    }

    #[test]
    fn managed_bridge_sweep_flushes_history_without_private_state() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("kimi");
        for name in ["session-a", "session-b"] {
            let bridge = primary.join(BRIDGE_PARENT).join(name);
            fs::create_dir_all(bridge.join("sessions/wd/session")).unwrap();
            fs::write(
                bridge.join("sessions/wd/session/wire.jsonl"),
                if name == "session-a" {
                    "{\"bridge\":\"base\"}\n".to_string()
                } else {
                    "{\"bridge\":\"base\"}\n{\"bridge\":\"session-b\"}\n".to_string()
                },
            )
            .unwrap();
            fs::write(bridge.join("server.token"), format!("secret-{name}")).unwrap();
            fs::write(bridge.join("mcp.json"), r#"{"bearer":"secret"}"#).unwrap();
        }

        sync_managed_bridges_to_primary(&primary).unwrap();
        let wire = fs::read_to_string(primary.join("sessions/wd/session/wire.jsonl")).unwrap();
        assert_eq!(wire, "{\"bridge\":\"base\"}\n{\"bridge\":\"session-b\"}\n");
        assert!(!primary.join("server.token").exists());
        assert!(!primary.join("mcp.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn prepare_rejects_preplanted_bridge_and_parent_symlinks_without_touching_targets() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("kimi");
        let bridge_parent = primary.join(BRIDGE_PARENT);
        fs::create_dir_all(&bridge_parent).unwrap();
        let protected_bridge = temp.path().join("protected-bridge");
        fs::create_dir_all(&protected_bridge).unwrap();
        fs::write(protected_bridge.join("keep"), "untouched").unwrap();
        symlink(&protected_bridge, bridge_parent.join(bridge_key("session"))).unwrap();

        let error = prepare_bridge_home(&primary, "session", None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read_to_string(protected_bridge.join("keep")).unwrap(),
            "untouched"
        );

        fs::remove_dir_all(&primary).unwrap();
        fs::create_dir_all(&primary).unwrap();
        let protected_parent = temp.path().join("protected-parent");
        fs::create_dir_all(&protected_parent).unwrap();
        fs::write(protected_parent.join("keep"), "still untouched").unwrap();
        symlink(&protected_parent, primary.join(BRIDGE_PARENT)).unwrap();

        let error = prepare_bridge_home(&primary, "session", None).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read_to_string(protected_parent.join("keep")).unwrap(),
            "still untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sync_rejects_a_bridge_root_replaced_by_a_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("kimi");
        let bridge_parent = primary.join(BRIDGE_PARENT);
        fs::create_dir_all(&bridge_parent).unwrap();
        let protected = temp.path().join("protected");
        fs::create_dir_all(&protected).unwrap();
        fs::write(protected.join("session_index.jsonl"), "protected").unwrap();
        let bridge = bridge_parent.join("session-test");
        symlink(&protected, &bridge).unwrap();

        let error = sync_bridge_home_to_primary(&bridge).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read_to_string(protected.join("session_index.jsonl")).unwrap(),
            "protected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn refresh_replaces_a_misdirected_entry_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("kimi");
        let identity = "session";
        let bridge = primary.join(BRIDGE_PARENT).join(bridge_key(identity));
        fs::create_dir_all(&bridge).unwrap();
        fs::write(primary.join("config.toml"), "model = \"current\"\n").unwrap();
        let protected = temp.path().join("protected");
        fs::write(&protected, "do not overwrite").unwrap();
        symlink(&protected, bridge.join("config.toml")).unwrap();

        prepare_bridge_home(&primary, identity, None).unwrap();

        assert_eq!(fs::read_to_string(&protected).unwrap(), "do not overwrite");
        assert_eq!(
            fs::canonicalize(bridge.join("config.toml")).unwrap(),
            fs::canonicalize(primary.join("config.toml")).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn merged_mcp_never_opens_a_predictable_preplanted_temporary_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("kimi");
        let identity = "session";
        let bridge = primary.join(BRIDGE_PARENT).join(bridge_key(identity));
        fs::create_dir_all(&bridge).unwrap();
        let protected = temp.path().join("protected");
        fs::write(&protected, "do not overwrite").unwrap();
        let predictable = bridge.join(format!("mcp.json.tmp-{}", std::process::id()));
        symlink(&protected, &predictable).unwrap();

        prepare_bridge_home(&primary, identity, None).unwrap();

        assert_eq!(fs::read_to_string(&protected).unwrap(), "do not overwrite");
        assert!(!predictable.exists());
        assert!(bridge.join("mcp.json").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn copy_fallback_sync_never_follows_destination_or_source_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("kimi");
        let bridge = primary.join(BRIDGE_PARENT).join("session-test");
        fs::create_dir_all(&bridge).unwrap();
        fs::create_dir_all(&primary).unwrap();

        let protected = temp.path().join("protected");
        fs::write(&protected, "primary").unwrap();
        symlink(&protected, primary.join("session_index.jsonl")).unwrap();
        fs::write(bridge.join("session_index.jsonl"), "bridge").unwrap();

        fs::create_dir_all(bridge.join("sessions")).unwrap();
        symlink(
            bridge.join("sessions"),
            bridge.join("sessions/recursive-link"),
        )
        .unwrap();

        sync_bridge_home_to_primary(&bridge).unwrap();
        assert_eq!(fs::read_to_string(&protected).unwrap(), "primary");
        assert!(!primary.join("sessions/recursive-link").exists());
    }

    #[test]
    fn copy_fallback_sync_refuses_unmanaged_paths() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(
            sync_bridge_home_to_primary(temp.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn concurrent_compatible_copy_fallbacks_merge_journals_and_agent_state() {
        let temp = tempfile::tempdir().unwrap();
        let primary = temp.path().join("kimi");
        let bridge_a = primary.join(BRIDGE_PARENT).join("session-a");
        let bridge_b = primary.join(BRIDGE_PARENT).join("session-b");
        for (bridge, suffix) in [(&bridge_a, "a"), (&bridge_b, "b")] {
            let session = bridge.join("sessions/workspace/session");
            fs::create_dir_all(&session).unwrap();
            let mut index = session_index_entry(bridge, "workspace", "base", temp.path());
            index.push_str(&session_index_entry(bridge, "workspace", "a", temp.path()));
            if suffix == "b" {
                index.push_str(&session_index_entry(bridge, "workspace", "b", temp.path()));
            }
            fs::write(bridge.join("session_index.jsonl"), index).unwrap();
            fs::write(
                session.join("wire.jsonl"),
                if suffix == "a" {
                    "{\"type\":\"base\"}\n{\"type\":\"a\"}\n".to_string()
                } else {
                    "{\"type\":\"base\"}\n{\"type\":\"a\"}\n{\"type\":\"b\"}\n".to_string()
                },
            )
            .unwrap();
            fs::write(
                session.join("state.json"),
                serde_json::to_vec(&serde_json::json!({
                    "id": "session",
                    "agents": {
                        "main": {"type": "main"},
                        suffix: {"type": "subagent", "name": suffix},
                    }
                }))
                .unwrap(),
            )
            .unwrap();
        }

        let barrier = Arc::new(Barrier::new(3));
        let handles = [bridge_a, bridge_b].map(|bridge| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                sync_bridge_home_to_primary(&bridge)
            })
        });
        barrier.wait();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }

        let index = read_session_index(&primary.join("session_index.jsonl"));
        assert_eq!(
            index
                .iter()
                .map(|record| record["sessionId"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["base", "a", "b"]
        );
        assert!(index.iter().all(|record| record["sessionDir"]
            .as_str()
            .unwrap()
            .starts_with(primary.join("sessions").to_string_lossy().as_ref())));
        let session = primary.join("sessions/workspace/session");
        let wire = fs::read_to_string(session.join("wire.jsonl")).unwrap();
        assert_eq!(
            wire,
            "{\"type\":\"base\"}\n{\"type\":\"a\"}\n{\"type\":\"b\"}\n"
        );
        let state: Value =
            serde_json::from_slice(&fs::read(session.join("state.json")).unwrap()).unwrap();
        assert_eq!(state["agents"]["main"]["type"], "main");
        assert_eq!(state["agents"]["a"]["name"], "a");
        assert_eq!(state["agents"]["b"]["name"], "b");

        // The lock file is intentionally durable: unlinking it while another
        // process already has the old inode open can create two independently
        // lockable files. The OS lock itself must be released after both syncs.
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(primary.join(SYNC_LOCK_NAME))
            .unwrap();
        fs::File::try_lock(&lock).unwrap();
        fs::File::unlock(&lock).unwrap();
    }

    #[test]
    fn journal_merge_ignores_an_incomplete_source_tail() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.jsonl");
        let destination = temp.path().join("destination.jsonl");
        fs::write(&source, b"{\"id\":0}\n{\"id\":1}\n{\"id\":2").unwrap();
        fs::write(&destination, b"{\"id\":0}\n").unwrap();
        merge_append_only_jsonl(&source, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination).unwrap(),
            "{\"id\":0}\n{\"id\":1}\n"
        );
    }

    #[test]
    fn journal_merge_preserves_ordered_prefixes_and_duplicate_records() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.jsonl");
        let destination = temp.path().join("destination.jsonl");
        let complete = b"{\"id\":\"a\"}\n{\"id\":\"a\"}\n{\"id\":\"b\"}\n{\"id\":\"b\"}\n";
        fs::write(&source, complete).unwrap();
        fs::write(&destination, b"{\"id\":\"a\"}\n{\"id\":\"a\"}\n").unwrap();

        merge_append_only_jsonl(&source, &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), complete);

        // The inverse strict-prefix relation is also safe: an older bridge
        // must not truncate or duplicate records already in the primary.
        fs::write(&source, b"{\"id\":\"a\"}\n{\"id\":\"a\"}\n").unwrap();
        merge_append_only_jsonl(&source, &destination).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), complete);
    }

    #[test]
    fn journal_merge_rejects_divergence_without_reordering_the_primary() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.jsonl");
        let destination = temp.path().join("destination.jsonl");
        let original = b"{\"id\":\"a\"}\n{\"id\":\"c\"}\n";
        fs::write(&source, b"{\"id\":\"a\"}\n{\"id\":\"b\"}\n{\"id\":\"c\"}\n").unwrap();
        fs::write(&destination, original).unwrap();

        let error = merge_append_only_jsonl(&source, &destination).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&destination).unwrap(), original);
        assert_ne!(
            fs::read_to_string(&destination).unwrap(),
            "{\"id\":\"a\"}\n{\"id\":\"c\"}\n{\"id\":\"b\"}\n"
        );
    }

    #[test]
    fn journal_merge_revalidates_a_primary_append_after_comparison() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.jsonl");
        let destination = temp.path().join("destination.jsonl");
        fs::write(&source, b"{\"id\":\"a\"}\n{\"id\":\"b\"}\n").unwrap();
        fs::write(&destination, b"{\"id\":\"a\"}\n").unwrap();

        let error = merge_append_only_jsonl_with_before_append(&source, &destination, || {
            let mut primary = OpenOptions::new().append(true).open(&destination).unwrap();
            primary.write_all(b"{\"id\":\"c\"}\n").unwrap();
            primary.sync_data().unwrap();
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read_to_string(&destination).unwrap(),
            "{\"id\":\"a\"}\n{\"id\":\"c\"}\n"
        );
    }

    #[test]
    fn journal_merge_revalidates_source_replacement_after_comparison() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.jsonl");
        let destination = temp.path().join("destination.jsonl");
        fs::write(&source, b"{\"id\":\"a\"}\n{\"id\":\"b\"}\n").unwrap();
        fs::write(&destination, b"{\"id\":\"a\"}\n").unwrap();

        let error = merge_append_only_jsonl_with_before_append(&source, &destination, || {
            fs::write(&source, b"{\"id\":\"a\"}\n{\"id\":\"x\"}\n").unwrap();
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read_to_string(&destination).unwrap(),
            "{\"id\":\"a\"}\n"
        );
    }
}
