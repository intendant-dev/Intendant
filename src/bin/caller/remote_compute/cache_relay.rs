//! Capability-scoped durable compiler cache relay.
//!
//! Codex Cloud's managed HTTP egress is not a byte-transparent path for all
//! signed object-store requests.  A remote command therefore gets a tiny,
//! job-scoped WebDAV endpoint on loopback.  The endpoint carries bounded
//! cache objects over the already-authenticated Cloud attachment and the home
//! daemon stores them under its private state root.  The worker never receives
//! home filesystem authority or durable storage credentials.

use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use axum::Router;
use base64::Engine as _;
use serde_json::{json, Value};
use sha2::Digest as _;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::{mpsc, oneshot};

pub(super) const REMOTE_CACHE_REQUEST_KIND: &str = "remote_cache_request";
pub(super) const REMOTE_CACHE_RESPONSE_KIND: &str = "remote_cache_response";

const CACHE_CHUNK_BYTES: usize = 192 * 1024;
const MAX_CACHE_OBJECT_BYTES: usize = 128 * 1024 * 1024;
const MAX_CACHE_WRITES_PER_JOB: u64 = 8 * 1024 * 1024 * 1024;
const DEFAULT_HOME_CACHE_BYTES: u64 = 20 * 1024 * 1024 * 1024;
const MIN_HOME_CACHE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_HOME_CACHE_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const RELAY_FRAME_CAP: usize = 16;
const RELAY_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TRANSFERS_PER_JOB: usize = 2;

const HOME_CACHE_DIR_ENV: &str = "INTENDANT_REMOTE_CACHE_HOME_DIR";
const HOME_CACHE_MAX_BYTES_ENV: &str = "INTENDANT_REMOTE_CACHE_MAX_BYTES";

#[derive(Clone)]
pub(super) struct HomeCacheStore {
    inner: Arc<HomeCacheStoreInner>,
}

struct HomeCacheStoreInner {
    root: PathBuf,
    max_bytes: u64,
    accounting: tokio::sync::Mutex<StoreAccounting>,
}

#[derive(Default)]
struct StoreAccounting {
    total_bytes: Option<u64>,
}

type StoreKey = (PathBuf, u64);
type StoreMap = HashMap<StoreKey, Weak<HomeCacheStoreInner>>;

static HOME_CACHE_STORES: OnceLock<Mutex<StoreMap>> = OnceLock::new();

impl HomeCacheStore {
    pub(super) fn for_project(project_root: &Path) -> Result<Self, String> {
        let identity = project_identity(project_root)?;
        let base = match std::env::var_os(HOME_CACHE_DIR_ENV) {
            Some(raw) if !raw.is_empty() => {
                let path = PathBuf::from(raw);
                if !path.is_absolute() {
                    return Err(format!("{HOME_CACHE_DIR_ENV} must be an absolute path"));
                }
                path
            }
            _ => crate::platform::intendant_home()
                .join("remote-cache")
                .join("sccache-v1"),
        };
        let max_bytes = parse_cache_size_env()?;
        Self::new(base.join(identity), max_bytes)
    }

    fn new(root: PathBuf, max_bytes: u64) -> Result<Self, String> {
        intendant_core::state_paths::create_private_dir_all(&root)
            .map_err(|error| format!("create home compiler cache {}: {error}", root.display()))?;
        harden_private_path(&root)
            .map_err(|error| format!("protect home compiler cache {}: {error}", root.display()))?;
        let key = (root.clone(), max_bytes);
        let inner = {
            let mut stores = HOME_CACHE_STORES
                .get_or_init(|| Mutex::new(HashMap::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            stores.retain(|_, store| store.strong_count() > 0);
            stores.get(&key).and_then(Weak::upgrade).unwrap_or_else(|| {
                let inner = Arc::new(HomeCacheStoreInner {
                    root,
                    max_bytes,
                    accounting: tokio::sync::Mutex::new(StoreAccounting::default()),
                });
                stores.insert(key, Arc::downgrade(&inner));
                inner
            })
        };
        Ok(Self { inner })
    }

    #[cfg(test)]
    fn new_for_test(root: PathBuf, max_bytes: u64) -> Self {
        Self::new(root, max_bytes).expect("test cache store")
    }

    fn path_for_key(&self, key: &str) -> Result<PathBuf, String> {
        validate_cache_key(key)?;
        Ok(self.inner.root.join(key))
    }

    async fn stat(&self, key: &str) -> Result<Option<u64>, String> {
        let path = self.path_for_key(key)?;
        match tokio::fs::metadata(&path).await {
            Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
            Ok(_) => Err("cache key did not resolve to a regular file".to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("inspect cached object: {error}")),
        }
    }

    async fn open(&self, key: &str) -> Result<Option<(tokio::fs::File, u64)>, String> {
        let path = self.path_for_key(key)?;
        match tokio::fs::File::open(&path).await {
            Ok(file) => {
                let metadata = file
                    .metadata()
                    .await
                    .map_err(|error| format!("inspect cached object: {error}"))?;
                if !metadata.is_file() {
                    return Err("cache key did not resolve to a regular file".to_string());
                }
                Ok(Some((file, metadata.len())))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("open cached object: {error}")),
        }
    }

    fn create_upload_file(&self, transfer_id: &str) -> Result<(PathBuf, tokio::fs::File), String> {
        validate_transfer_id(transfer_id)?;
        let temp_dir = self.inner.root.join(".tmp");
        intendant_core::state_paths::create_private_dir_all(&temp_dir)
            .map_err(|error| format!("create cache staging directory: {error}"))?;
        harden_private_path(&temp_dir)
            .map_err(|error| format!("protect cache staging directory: {error}"))?;
        let path = temp_dir.join(format!("{transfer_id}.part"));
        let file = intendant_core::state_paths::private_file_options()
            .create_new(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| format!("create cache staging file: {error}"))?;
        harden_private_path(&path)
            .map_err(|error| format!("protect cache staging file: {error}"))?;
        Ok((path, tokio::fs::File::from_std(file)))
    }

    async fn commit(&self, temp: PathBuf, key: String, size: u64) -> Result<(), String> {
        let destination = self.path_for_key(&key)?;
        let mut accounting = self.inner.accounting.lock().await;
        if accounting.total_bytes.is_none() {
            let root = self.inner.root.clone();
            accounting.total_bytes = Some(
                tokio::task::spawn_blocking(move || cache_total_bytes(&root))
                    .await
                    .map_err(|error| format!("scan home compiler cache task: {error}"))??,
            );
        }
        let current_total = accounting.total_bytes.unwrap_or_default();
        let root = self.inner.root.clone();
        let max_bytes = self.inner.max_bytes;
        let cleanup_temp = temp.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            commit_cache_file(&root, &temp, &destination, size, current_total, max_bytes)
        })
        .await
        .map_err(|error| format!("commit home compiler cache task: {error}"))?;
        match outcome {
            Ok(total) => {
                accounting.total_bytes = Some(total);
                Ok(())
            }
            Err(error) => {
                accounting.total_bytes = None;
                let _ = std::fs::remove_file(&cleanup_temp);
                Err(error)
            }
        }
    }

    async fn delete(&self, key: &str) -> Result<bool, String> {
        let path = self.path_for_key(key)?;
        let mut accounting = self.inner.accounting.lock().await;
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(format!("inspect cached object before delete: {error}")),
        };
        std::fs::remove_file(&path).map_err(|error| format!("delete cached object: {error}"))?;
        if let Some(total) = accounting.total_bytes.as_mut() {
            *total = total.saturating_sub(metadata.len());
        }
        Ok(true)
    }
}

fn project_identity(project_root: &Path) -> Result<String, String> {
    let root = project_root
        .canonicalize()
        .map_err(|error| format!("resolve cache project root: {error}"))?;
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .map_err(|error| format!("resolve Git common directory for compiler cache: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "resolve Git common directory for compiler cache: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if raw.is_empty() {
        return Err("Git returned an empty common directory for compiler cache".to_string());
    }
    let common = PathBuf::from(raw);
    let common = if common.is_absolute() {
        common
    } else {
        root.join(common)
    };
    let common = common
        .canonicalize()
        .map_err(|error| format!("resolve Git common directory for compiler cache: {error}"))?;
    Ok(sha256_hex(common.to_string_lossy().as_bytes()))
}

fn parse_cache_size_env() -> Result<u64, String> {
    let Some(raw) = std::env::var(HOME_CACHE_MAX_BYTES_ENV).ok() else {
        return Ok(DEFAULT_HOME_CACHE_BYTES);
    };
    let bytes = raw
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("{HOME_CACHE_MAX_BYTES_ENV} must be an integer byte count"))?;
    if !(MIN_HOME_CACHE_BYTES..=MAX_HOME_CACHE_BYTES).contains(&bytes) {
        return Err(format!(
            "{HOME_CACHE_MAX_BYTES_ENV} must be between {MIN_HOME_CACHE_BYTES} and {MAX_HOME_CACHE_BYTES} bytes"
        ));
    }
    Ok(bytes)
}

fn harden_private_path(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        crate::platform::set_owner_private_permissions(path)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(())
    }
}

fn cache_total_bytes(root: &Path) -> Result<u64, String> {
    Ok(cache_files(root)?
        .into_iter()
        .map(|file| file.bytes)
        .fold(0u64, u64::saturating_add))
}

struct CacheFile {
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

fn cache_files(root: &Path) -> Result<Vec<CacheFile>, String> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|error| format!("scan compiler cache {}: {error}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("scan compiler cache entry: {error}"))?;
            if entry.file_name() == ".tmp" {
                continue;
            }
            let file_type = entry
                .file_type()
                .map_err(|error| format!("inspect compiler cache entry type: {error}"))?;
            if file_type.is_symlink() {
                return Err(format!(
                    "compiler cache contains an unexpected symlink at {}",
                    entry.path().display()
                ));
            }
            let metadata = std::fs::symlink_metadata(entry.path())
                .map_err(|error| format!("inspect compiler cache entry: {error}"))?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_file() {
                files.push(CacheFile {
                    path: entry.path(),
                    bytes: metadata.len(),
                    modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                });
            }
        }
    }
    Ok(files)
}

fn commit_cache_file(
    root: &Path,
    temp: &Path,
    destination: &Path,
    size: u64,
    current_total: u64,
    max_bytes: u64,
) -> Result<u64, String> {
    if size > max_bytes {
        return Err(format!(
            "cache object is {size} bytes, larger than the {max_bytes}-byte home cache"
        ));
    }
    let replaced = std::fs::metadata(destination)
        .ok()
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    let mut projected = current_total.saturating_sub(replaced).saturating_add(size);
    if projected > max_bytes {
        let mut files = cache_files(root)?;
        files.sort_by_key(|file| file.modified);
        for file in files {
            if file.path == destination {
                continue;
            }
            match std::fs::remove_file(&file.path) {
                Ok(()) => projected = projected.saturating_sub(file.bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "evict compiler cache object {}: {error}",
                        file.path.display()
                    ))
                }
            }
            if projected <= max_bytes {
                break;
            }
        }
    }
    if projected > max_bytes {
        return Err("home compiler cache could not free enough space for an object".to_string());
    }
    let parent = destination
        .parent()
        .ok_or_else(|| "cache destination has no parent directory".to_string())?;
    intendant_core::state_paths::create_private_dir_all(parent)
        .map_err(|error| format!("create compiler cache object directory: {error}"))?;
    harden_private_path(parent)
        .map_err(|error| format!("protect compiler cache object directory: {error}"))?;
    if let Err(first_error) = std::fs::rename(temp, destination) {
        // Unix rename replaces an existing file atomically; Windows does not.
        // The store-wide commit lock makes the remove+rename compatibility
        // fallback safe, and a concurrent reader merely observes a cache miss.
        if destination.is_file() {
            std::fs::remove_file(destination).map_err(|error| {
                format!("replace existing compiler cache object after {first_error}: {error}")
            })?;
            std::fs::rename(temp, destination)
                .map_err(|error| format!("commit replacement compiler cache object: {error}"))?;
        } else {
            return Err(format!(
                "atomically commit compiler cache object: {first_error}"
            ));
        }
    }
    harden_private_path(destination)
        .map_err(|error| format!("protect compiler cache object: {error}"))?;
    Ok(projected)
}

fn validate_cache_key(key: &str) -> Result<(), String> {
    if key == ".sccache_check" {
        return Ok(());
    }
    if key.is_empty() || key.len() > 512 || key.starts_with('.') || key.contains('%') {
        return Err("cache key has an invalid shape".to_string());
    }
    let path = Path::new(key);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("cache key must be a safe relative path".to_string());
    }
    let parts = key.split('/').collect::<Vec<_>>();
    let valid = (2..=8).contains(&parts.len())
        && parts[..parts.len() - 1]
            .iter()
            .all(|part| !part.is_empty() && part.len() <= 16 && part.bytes().all(hex_byte))
        && parts
            .last()
            .is_some_and(|part| (32..=128).contains(&part.len()) && part.bytes().all(hex_byte));
    if !valid {
        return Err("cache key is not a content-addressed sccache path".to_string());
    }
    Ok(())
}

fn validate_cache_directory(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Ok(());
    }
    if path.len() > 256 || path.contains('%') {
        return Err("cache directory has an invalid shape".to_string());
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part.len() > 16 || !part.bytes().all(hex_byte))
    {
        return Err("cache directory is not a safe sccache prefix".to_string());
    }
    Ok(())
}

fn hex_byte(byte: u8) -> bool {
    byte.is_ascii_hexdigit()
}

fn validate_transfer_id(id: &str) -> Result<(), String> {
    if id.len() == 32 && id.bytes().all(hex_byte) {
        Ok(())
    } else {
        Err("cache transfer id has an invalid shape".to_string())
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct RelayRoute {
    token: String,
    frames: mpsc::Sender<Value>,
}

type RelayKey = (String, String);
static HOME_RELAYS: OnceLock<Mutex<HashMap<RelayKey, RelayRoute>>> = OnceLock::new();

fn home_relays() -> &'static Mutex<HashMap<RelayKey, RelayRoute>> {
    HOME_RELAYS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Route a cache frame into its one active, home-authorized job.  Cache
/// frames never enter the general worker-reply broadcast.
pub(super) fn route_worker_frame(task_id: &str, text: &str) -> bool {
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    if frame.get("t").and_then(Value::as_str) != Some(REMOTE_CACHE_REQUEST_KIND) {
        return false;
    }
    let job_id = frame.get("id").and_then(Value::as_str).unwrap_or_default();
    let token = frame
        .get("relay_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let relays = home_relays()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(route) = relays.get(&(task_id.to_string(), job_id.to_string())) else {
        return true;
    };
    if !constant_time_eq(route.token.as_bytes(), token.as_bytes()) {
        return true;
    }
    let _ = route.frames.try_send(frame);
    true
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

pub(super) struct HomeCacheSession {
    key: RelayKey,
    token: String,
    host_id: String,
    store: HomeCacheStore,
    frames: mpsc::Receiver<Value>,
    uploads: HashMap<String, HomeUpload>,
    downloads: HashMap<String, HomeDownload>,
    committed_bytes: u64,
}

struct HomeUpload {
    key: String,
    temp_path: PathBuf,
    file: tokio::fs::File,
    expected_bytes: u64,
    expected_digest: String,
    written: u64,
    digest: sha2::Sha256,
}

struct HomeDownload {
    file: tokio::fs::File,
    bytes: u64,
    offset: u64,
}

impl HomeCacheSession {
    pub(super) fn register(
        task_id: &str,
        job_id: &str,
        token: &str,
        store: HomeCacheStore,
    ) -> Result<Self, String> {
        validate_relay_token(token)?;
        let key = (task_id.to_string(), job_id.to_string());
        let (frames_tx, frames) = mpsc::channel(RELAY_FRAME_CAP);
        let mut relays = home_relays()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if relays.contains_key(&key) {
            return Err("a cache relay is already registered for this remote job".to_string());
        }
        relays.insert(
            key.clone(),
            RelayRoute {
                token: token.to_string(),
                frames: frames_tx,
            },
        );
        drop(relays);
        Ok(Self {
            key,
            token: token.to_string(),
            host_id: format!(
                "{}{}",
                super::super::codex_cloud_attach::CLOUD_HOST_PREFIX,
                task_id
            ),
            store,
            frames,
            uploads: HashMap::new(),
            downloads: HashMap::new(),
            committed_bytes: 0,
        })
    }

    pub(super) async fn next_frame(&mut self) -> Option<Value> {
        self.frames.recv().await
    }

    pub(super) async fn handle_frame(&mut self, frame: Value, to_worker: &mpsc::Sender<String>) {
        let request_id = frame
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let response = if validate_request_id(&request_id).is_err() {
            error_response("cache relay request id has an invalid shape")
        } else {
            match frame.get("op").and_then(Value::as_str).unwrap_or_default() {
                "stat" => self.stat(&frame).await,
                "get_begin" => self.get_begin(&frame).await,
                "get_chunk" => self.get_chunk(&frame).await,
                "put_begin" => self.put_begin(&frame).await,
                "put_chunk" => self.put_chunk(&frame).await,
                "put_finish" => self.put_finish(&frame).await,
                "delete" => self.delete(&frame).await,
                _ => error_response("cache relay operation is not supported"),
            }
        };
        let mut response = response;
        if let Some(map) = response.as_object_mut() {
            map.insert("t".into(), REMOTE_CACHE_RESPONSE_KIND.into());
            map.insert("host_id".into(), self.host_id.clone().into());
            map.insert("id".into(), self.key.1.clone().into());
            map.insert("request_id".into(), request_id.into());
        }
        let _ = super::send_home_frame(to_worker, response.to_string()).await;
    }

    async fn stat(&self, frame: &Value) -> Value {
        let key = frame_string(frame, "key");
        match self.store.stat(&key).await {
            Ok(Some(bytes)) => json!({"state":"hit", "bytes":bytes}),
            Ok(None) => json!({"state":"miss"}),
            Err(error) => error_response(error),
        }
    }

    async fn get_begin(&mut self, frame: &Value) -> Value {
        if self.downloads.len() >= MAX_TRANSFERS_PER_JOB {
            return error_response("too many cache downloads are active for this job");
        }
        let key = frame_string(frame, "key");
        let transfer_id = frame_string(frame, "transfer_id");
        if let Err(error) = validate_transfer_id(&transfer_id) {
            return error_response(error);
        }
        match self.store.open(&key).await {
            Ok(Some((file, bytes))) => {
                if bytes > MAX_CACHE_OBJECT_BYTES as u64 {
                    return error_response("cached object exceeds the relay object limit");
                }
                if bytes > 0 {
                    self.downloads.insert(
                        transfer_id.clone(),
                        HomeDownload {
                            file,
                            bytes,
                            offset: 0,
                        },
                    );
                }
                json!({"state":"hit", "transfer_id":transfer_id, "bytes":bytes})
            }
            Ok(None) => json!({"state":"miss"}),
            Err(error) => error_response(error),
        }
    }

    async fn get_chunk(&mut self, frame: &Value) -> Value {
        let transfer_id = frame_string(frame, "transfer_id");
        let offset = frame.get("offset").and_then(Value::as_u64);
        let Some(download) = self.downloads.get_mut(&transfer_id) else {
            return error_response("cache download was not begun");
        };
        if offset != Some(download.offset) {
            self.downloads.remove(&transfer_id);
            return error_response("cache download chunk is out of order");
        }
        let remaining = download.bytes.saturating_sub(download.offset);
        let mut bytes = vec![0u8; remaining.min(CACHE_CHUNK_BYTES as u64) as usize];
        if let Err(error) = download.file.read_exact(&mut bytes).await {
            self.downloads.remove(&transfer_id);
            return error_response(format!("read cached object: {error}"));
        }
        let chunk_offset = download.offset;
        download.offset = download.offset.saturating_add(bytes.len() as u64);
        let done = download.offset == download.bytes;
        if done {
            self.downloads.remove(&transfer_id);
        }
        json!({
            "state":"chunk",
            "offset":chunk_offset,
            "data":base64::engine::general_purpose::STANDARD.encode(bytes),
            "done":done,
        })
    }

    async fn put_begin(&mut self, frame: &Value) -> Value {
        if self.uploads.len() >= MAX_TRANSFERS_PER_JOB {
            return error_response("too many cache uploads are active for this job");
        }
        let key = frame_string(frame, "key");
        if let Err(error) = validate_cache_key(&key) {
            return error_response(error);
        }
        let transfer_id = frame_string(frame, "transfer_id");
        if let Err(error) = validate_transfer_id(&transfer_id) {
            return error_response(error);
        }
        let Some(expected_bytes) = frame.get("bytes").and_then(Value::as_u64) else {
            return error_response("cache upload carried no byte count");
        };
        if expected_bytes > MAX_CACHE_OBJECT_BYTES as u64 {
            return error_response("cache upload exceeds the relay object limit");
        }
        if self.committed_bytes.saturating_add(expected_bytes) > MAX_CACHE_WRITES_PER_JOB {
            return error_response("cache upload exceeds the per-job write allowance");
        }
        let expected_digest = frame_string(frame, "sha256");
        if expected_digest.len() != 64 || !expected_digest.bytes().all(hex_byte) {
            return error_response("cache upload digest has an invalid shape");
        }
        let (temp_path, file) = match self.store.create_upload_file(&transfer_id) {
            Ok(created) => created,
            Err(error) => return error_response(error),
        };
        self.uploads.insert(
            transfer_id.clone(),
            HomeUpload {
                key,
                temp_path,
                file,
                expected_bytes,
                expected_digest,
                written: 0,
                digest: sha2::Sha256::new(),
            },
        );
        json!({"state":"ready", "transfer_id":transfer_id, "offset":0})
    }

    async fn put_chunk(&mut self, frame: &Value) -> Value {
        let transfer_id = frame_string(frame, "transfer_id");
        let offset = frame.get("offset").and_then(Value::as_u64);
        let data = frame
            .get("data")
            .and_then(Value::as_str)
            .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok());
        let Some(upload) = self.uploads.get_mut(&transfer_id) else {
            return error_response("cache upload was not begun");
        };
        let valid = offset == Some(upload.written)
            && data.as_ref().is_some_and(|data| {
                data.len() <= CACHE_CHUNK_BYTES
                    && upload.written.saturating_add(data.len() as u64) <= upload.expected_bytes
            });
        if !valid {
            let upload = self.uploads.remove(&transfer_id).expect("upload exists");
            discard_upload(upload);
            return error_response("cache upload chunk is invalid or out of order");
        }
        let data = data.unwrap_or_default();
        if let Err(error) = upload.file.write_all(&data).await {
            let upload = self.uploads.remove(&transfer_id).expect("upload exists");
            discard_upload(upload);
            return error_response(format!("write cache upload: {error}"));
        }
        upload.digest.update(&data);
        upload.written = upload.written.saturating_add(data.len() as u64);
        json!({"state":"ready", "transfer_id":transfer_id, "offset":upload.written})
    }

    async fn put_finish(&mut self, frame: &Value) -> Value {
        let transfer_id = frame_string(frame, "transfer_id");
        let Some(mut upload) = self.uploads.remove(&transfer_id) else {
            return error_response("cache upload was not begun");
        };
        if upload.written != upload.expected_bytes {
            discard_upload(upload);
            return error_response("cache upload finished at the wrong byte count");
        }
        if let Err(error) = upload.file.flush().await {
            discard_upload(upload);
            return error_response(format!("flush cache upload: {error}"));
        }
        if let Err(error) = upload.file.sync_all().await {
            discard_upload(upload);
            return error_response(format!("sync cache upload: {error}"));
        }
        let actual_digest = upload
            .digest
            .clone()
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if actual_digest != upload.expected_digest {
            discard_upload(upload);
            return error_response("cache upload digest did not match its declared content");
        }
        let HomeUpload {
            key,
            temp_path,
            file,
            expected_bytes: bytes,
            ..
        } = upload;
        drop(file);
        match self.store.commit(temp_path, key, bytes).await {
            Ok(()) => {
                self.committed_bytes = self.committed_bytes.saturating_add(bytes);
                json!({"state":"stored", "bytes":bytes})
            }
            Err(error) => error_response(error),
        }
    }

    async fn delete(&self, frame: &Value) -> Value {
        let key = frame_string(frame, "key");
        match self.store.delete(&key).await {
            Ok(deleted) => json!({"state":"deleted", "deleted":deleted}),
            Err(error) => error_response(error),
        }
    }
}

impl Drop for HomeCacheSession {
    fn drop(&mut self) {
        let mut relays = home_relays()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if relays
            .get(&self.key)
            .is_some_and(|route| constant_time_eq(route.token.as_bytes(), self.token.as_bytes()))
        {
            relays.remove(&self.key);
        }
        drop(relays);
        for (_, upload) in self.uploads.drain() {
            discard_upload(upload);
        }
    }
}

fn discard_upload(upload: HomeUpload) {
    let path = upload.temp_path.clone();
    drop(upload);
    let _ = std::fs::remove_file(path);
}

fn frame_string(frame: &Value, key: &str) -> String {
    frame
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn error_response(error: impl Into<String>) -> Value {
    json!({"state":"error", "error":error.into()})
}

fn validate_relay_token(token: &str) -> Result<(), String> {
    if token.len() == 32 && token.bytes().all(hex_byte) {
        Ok(())
    } else {
        Err("cache relay token has an invalid shape".to_string())
    }
}

fn validate_request_id(id: &str) -> Result<(), String> {
    validate_transfer_id(id)
}

type PendingCacheResponses = HashMap<(String, String), oneshot::Sender<Value>>;

#[derive(Clone, Default)]
pub(super) struct WorkerCacheRelay {
    pending: Arc<tokio::sync::Mutex<PendingCacheResponses>>,
}

impl WorkerCacheRelay {
    pub(super) async fn serve_frame(&self, frame: &Value) -> bool {
        if frame.get("t").and_then(Value::as_str) != Some(REMOTE_CACHE_RESPONSE_KIND) {
            return false;
        }
        let job_id = frame_string(frame, "id");
        let request_id = frame_string(frame, "request_id");
        if let Some(sender) = self.pending.lock().await.remove(&(job_id, request_id)) {
            let _ = sender.send(frame.clone());
        }
        true
    }

    pub(super) fn client(
        &self,
        job_id: String,
        token: String,
        host_id: String,
        outbound: mpsc::Sender<String>,
    ) -> WorkerCacheClient {
        WorkerCacheClient {
            relay: self.clone(),
            job_id,
            token,
            host_id,
            outbound,
        }
    }

    pub(super) async fn cancel_job(&self, job_id: &str) {
        self.pending
            .lock()
            .await
            .retain(|(pending_job, _), _| pending_job != job_id);
    }
}

#[derive(Clone)]
pub(super) struct WorkerCacheClient {
    relay: WorkerCacheRelay,
    job_id: String,
    token: String,
    host_id: String,
    outbound: mpsc::Sender<String>,
}

impl WorkerCacheClient {
    async fn exchange(&self, op: &str, fields: Value) -> Result<Value, String> {
        let request_id = uuid::Uuid::new_v4().simple().to_string();
        let key = (self.job_id.clone(), request_id.clone());
        let (reply_tx, reply_rx) = oneshot::channel();
        self.relay
            .pending
            .lock()
            .await
            .insert(key.clone(), reply_tx);
        let mut frame = json!({
            "t":REMOTE_CACHE_REQUEST_KIND,
            "host_id":self.host_id,
            "id":self.job_id,
            "relay_token":self.token,
            "request_id":request_id,
            "op":op,
        });
        if let (Some(target), Some(source)) = (frame.as_object_mut(), fields.as_object()) {
            target.extend(source.clone());
        }
        let send =
            tokio::time::timeout(RELAY_REQUEST_TIMEOUT, self.outbound.send(frame.to_string()))
                .await;
        if !matches!(send, Ok(Ok(()))) {
            self.relay.pending.lock().await.remove(&key);
            return Err("cache relay attachment stopped accepting requests".to_string());
        }
        match tokio::time::timeout(RELAY_REQUEST_TIMEOUT, reply_rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err("cache relay response lane closed".to_string()),
            Err(_) => {
                self.relay.pending.lock().await.remove(&key);
                Err("cache relay request timed out".to_string())
            }
        }
    }

    async fn stat(&self, key: &str) -> Result<Option<u64>, String> {
        let response = self.exchange("stat", json!({"key":key})).await?;
        match response.get("state").and_then(Value::as_str) {
            Some("hit") => response
                .get("bytes")
                .and_then(Value::as_u64)
                .map(Some)
                .ok_or_else(|| "cache relay stat carried no byte count".to_string()),
            Some("miss") => Ok(None),
            _ => Err(response_error(&response)),
        }
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        let transfer_id = uuid::Uuid::new_v4().simple().to_string();
        let response = self
            .exchange("get_begin", json!({"key":key, "transfer_id":transfer_id}))
            .await?;
        let expected = match response.get("state").and_then(Value::as_str) {
            Some("miss") => return Ok(None),
            Some("hit") => response
                .get("bytes")
                .and_then(Value::as_u64)
                .and_then(|bytes| usize::try_from(bytes).ok())
                .filter(|bytes| *bytes <= MAX_CACHE_OBJECT_BYTES)
                .ok_or_else(|| "cache relay download size is invalid".to_string())?,
            _ => return Err(response_error(&response)),
        };
        let mut bytes = Vec::with_capacity(expected.min(4 * 1024 * 1024));
        while bytes.len() < expected {
            let response = self
                .exchange(
                    "get_chunk",
                    json!({"transfer_id":transfer_id, "offset":bytes.len()}),
                )
                .await?;
            if response.get("state").and_then(Value::as_str) != Some("chunk")
                || response.get("offset").and_then(Value::as_u64) != Some(bytes.len() as u64)
            {
                return Err(response_error(&response));
            }
            let chunk = response
                .get("data")
                .and_then(Value::as_str)
                .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok())
                .filter(|chunk| chunk.len() <= CACHE_CHUNK_BYTES)
                .ok_or_else(|| "cache relay download chunk is invalid".to_string())?;
            if chunk.is_empty() || bytes.len().saturating_add(chunk.len()) > expected {
                return Err("cache relay download made invalid progress".to_string());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some(bytes))
    }

    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > MAX_CACHE_OBJECT_BYTES {
            return Err("cache object exceeds the relay object limit".to_string());
        }
        let transfer_id = uuid::Uuid::new_v4().simple().to_string();
        let response = self
            .exchange(
                "put_begin",
                json!({
                    "key":key,
                    "transfer_id":transfer_id,
                    "bytes":bytes.len(),
                    "sha256":sha256_hex(bytes),
                }),
            )
            .await?;
        if response.get("state").and_then(Value::as_str) != Some("ready") {
            return Err(response_error(&response));
        }
        let mut offset = 0usize;
        for chunk in bytes.chunks(CACHE_CHUNK_BYTES) {
            let response = self
                .exchange(
                    "put_chunk",
                    json!({
                        "transfer_id":transfer_id,
                        "offset":offset,
                        "data":base64::engine::general_purpose::STANDARD.encode(chunk),
                    }),
                )
                .await?;
            offset = offset.saturating_add(chunk.len());
            if response.get("state").and_then(Value::as_str) != Some("ready")
                || response.get("offset").and_then(Value::as_u64) != Some(offset as u64)
            {
                return Err(response_error(&response));
            }
        }
        let response = self
            .exchange("put_finish", json!({"transfer_id":transfer_id}))
            .await?;
        if response.get("state").and_then(Value::as_str) == Some("stored") {
            Ok(())
        } else {
            Err(response_error(&response))
        }
    }

    async fn delete(&self, key: &str) -> Result<(), String> {
        let response = self.exchange("delete", json!({"key":key})).await?;
        if response.get("state").and_then(Value::as_str) == Some("deleted") {
            Ok(())
        } else {
            Err(response_error(&response))
        }
    }
}

fn response_error(response: &Value) -> String {
    response
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("cache relay returned an invalid response")
        .to_string()
}

pub(super) struct WorkerCacheSidecar {
    endpoint: String,
    shutdown: tokio_util::sync::CancellationToken,
}

impl WorkerCacheSidecar {
    pub(super) async fn start(client: WorkerCacheClient) -> Result<Self, String> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| format!("bind worker cache relay: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("read worker cache relay address: {error}"))?;
        let endpoint = format!("http://127.0.0.1:{}", address.port());
        let shutdown = tokio_util::sync::CancellationToken::new();
        let shutdown_task = shutdown.clone();
        let state = Arc::new(WebDavState {
            client,
            operation: tokio::sync::Mutex::new(()),
        });
        let app = Router::new().fallback(webdav_request).with_state(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_task.cancelled_owned())
                .await;
        });
        Ok(Self { endpoint, shutdown })
    }

    pub(super) fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl Drop for WorkerCacheSidecar {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

struct WebDavState {
    client: WorkerCacheClient,
    /// sccache may issue backend calls concurrently. Serializing the relay
    /// keeps both memory and attachment traffic bounded independently of the
    /// compiler's job count.
    operation: tokio::sync::Mutex<()>,
}

async fn webdav_request(
    State(state): State<Arc<WebDavState>>,
    method: Method,
    uri: Uri,
    _headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    let _operation = state.operation.lock().await;
    let raw_path = uri.path();
    let key = raw_path.strip_prefix('/').unwrap_or(raw_path);
    match method.as_str() {
        "GET" => match validate_cache_key(key) {
            Ok(()) => match state.client.get(key).await {
                Ok(Some(bytes)) => binary_response(StatusCode::OK, bytes),
                Ok(None) => empty_response(StatusCode::NOT_FOUND),
                Err(error) => {
                    eprintln!("[cloud-agent] compiler cache read degraded to a miss: {error}");
                    empty_response(StatusCode::NOT_FOUND)
                }
            },
            Err(_) => empty_response(StatusCode::BAD_REQUEST),
        },
        "HEAD" => match validate_cache_key(key) {
            Ok(()) => match state.client.stat(key).await {
                Ok(Some(bytes)) => sized_empty_response(StatusCode::OK, bytes),
                Ok(None) => empty_response(StatusCode::NOT_FOUND),
                Err(_) => empty_response(StatusCode::NOT_FOUND),
            },
            Err(_) => empty_response(StatusCode::BAD_REQUEST),
        },
        "PUT" => {
            if validate_cache_key(key).is_err() {
                return empty_response(StatusCode::BAD_REQUEST);
            }
            match to_bytes(body, MAX_CACHE_OBJECT_BYTES).await {
                Ok(bytes) => match state.client.put(key, &bytes).await {
                    Ok(()) => empty_response(StatusCode::CREATED),
                    Err(error) => {
                        eprintln!("[cloud-agent] compiler cache write failed: {error}");
                        empty_response(StatusCode::INSUFFICIENT_STORAGE)
                    }
                },
                Err(_) => empty_response(StatusCode::PAYLOAD_TOO_LARGE),
            }
        }
        "DELETE" => {
            if validate_cache_key(key).is_err() {
                return empty_response(StatusCode::BAD_REQUEST);
            }
            match state.client.delete(key).await {
                Ok(()) => empty_response(StatusCode::NO_CONTENT),
                Err(_) => empty_response(StatusCode::BAD_GATEWAY),
            }
        }
        "MKCOL" => {
            let directory = key.trim_end_matches('/');
            match validate_cache_directory(directory) {
                Ok(()) => empty_response(StatusCode::CREATED),
                Err(_) => empty_response(StatusCode::BAD_REQUEST),
            }
        }
        "PROPFIND" => {
            if raw_path == "/" || raw_path.ends_with('/') {
                let directory = key.trim_end_matches('/');
                return match validate_cache_directory(directory) {
                    Ok(()) => webdav_properties(raw_path, 0, true),
                    Err(_) => empty_response(StatusCode::BAD_REQUEST),
                };
            }
            match validate_cache_key(key) {
                Ok(()) => match state.client.stat(key).await {
                    Ok(Some(bytes)) => webdav_properties(raw_path, bytes, false),
                    Ok(None) | Err(_) => empty_response(StatusCode::NOT_FOUND),
                },
                Err(_) => empty_response(StatusCode::BAD_REQUEST),
            }
        }
        "OPTIONS" => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header("dav", "1")
            .header("allow", "GET, HEAD, PUT, DELETE, MKCOL, PROPFIND, OPTIONS")
            .body(Body::empty())
            .unwrap_or_else(|_| empty_response(StatusCode::INTERNAL_SERVER_ERROR)),
        _ => empty_response(StatusCode::METHOD_NOT_ALLOWED),
    }
}

fn empty_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, "0")
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn sized_empty_response(status: StatusCode, bytes: u64) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, bytes.to_string())
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn binary_response(status: StatusCode, bytes: Vec<u8>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .body(Body::from(bytes))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn webdav_properties(path: &str, bytes: u64, directory: bool) -> Response<Body> {
    let resource_type = if directory {
        "<D:resourcetype><D:collection/></D:resourcetype>"
    } else {
        "<D:resourcetype/>"
    };
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\"><D:response><D:href>{path}</D:href><D:propstat><D:prop>{resource_type}<D:getlastmodified>Sat, 01 Aug 2026 00:00:00 GMT</D:getlastmodified><D:getcontentlength>{bytes}</D:getcontentlength><D:getcontenttype>application/octet-stream</D:getcontenttype><D:getetag>\"intendant-cache\"</D:getetag></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"
    );
    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header(header::CONTENT_TYPE, "application/xml")
        .header(header::CONTENT_LENGTH, body.len().to_string())
        .body(Body::from(body))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_key(seed: char) -> String {
        let digest = std::iter::repeat_n(seed, 64).collect::<String>();
        format!("{0}/{0}/{0}/{digest}", seed)
    }

    #[test]
    fn cache_paths_are_content_addressed_and_never_traverse() {
        assert!(validate_cache_key(&cache_key('a')).is_ok());
        assert!(validate_cache_key(".sccache_check").is_ok());
        for invalid in [
            "../secret",
            "/absolute",
            ".tmp/file",
            "a/b/not-hex",
            "a/%2e%2e/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(validate_cache_key(invalid).is_err(), "{invalid}");
        }
        assert!(validate_cache_directory("").is_ok());
        assert!(validate_cache_directory("a/b/c").is_ok());
        assert!(validate_cache_directory("a/../b").is_err());
    }

    #[tokio::test]
    async fn home_cache_evicts_old_objects_before_crossing_its_ceiling() {
        let temp = tempfile::tempdir().unwrap();
        let store = HomeCacheStore::new_for_test(temp.path().join("cache"), 600);
        async fn commit(store: &HomeCacheStore, key: String, byte: u8) {
            let transfer_id = uuid::Uuid::new_v4().simple().to_string();
            let (path, mut file) = store.create_upload_file(&transfer_id).unwrap();
            let bytes = [byte; 400];
            file.write_all(&bytes).await.unwrap();
            file.sync_all().await.unwrap();
            drop(file);
            store.commit(path, key, 400).await.unwrap();
        }

        let first = cache_key('e');
        let second = cache_key('f');
        commit(&store, first.clone(), 1).await;
        commit(&store, second.clone(), 2).await;
        assert_eq!(store.stat(&first).await.unwrap(), None);
        assert_eq!(store.stat(&second).await.unwrap(), Some(400));
    }

    #[tokio::test]
    async fn capability_scoped_relay_round_trips_chunked_objects() {
        let temp = tempfile::tempdir().unwrap();
        let store = HomeCacheStore::new_for_test(temp.path().join("cache"), 4 * 1024 * 1024);
        let task = format!("task-test-{}", uuid::Uuid::new_v4());
        let job = format!("remote-test-{}", uuid::Uuid::new_v4());
        let token = uuid::Uuid::new_v4().simple().to_string();
        let mut home = HomeCacheSession::register(&task, &job, &token, store).unwrap();
        let (home_to_worker_tx, mut home_to_worker_rx) = mpsc::channel::<String>(64);
        let (worker_to_home_tx, mut worker_to_home_rx) = mpsc::channel::<String>(64);
        let worker = WorkerCacheRelay::default();
        let worker_inbound = worker.clone();
        let inbound_task = tokio::spawn(async move {
            while let Some(text) = home_to_worker_rx.recv().await {
                let frame: Value = serde_json::from_str(&text).unwrap();
                worker_inbound.serve_frame(&frame).await;
            }
        });
        let route_task_id = task.clone();
        let route_task = tokio::spawn(async move {
            while let Some(text) = worker_to_home_rx.recv().await {
                assert!(route_worker_frame(&route_task_id, &text));
            }
        });
        let home_task = tokio::spawn(async move {
            while let Some(frame) = home.next_frame().await {
                home.handle_frame(frame, &home_to_worker_tx).await;
            }
        });
        let client = worker.client(job, token, format!("cloud:{task}"), worker_to_home_tx);
        let key = cache_key('b');
        let body = vec![0x5a; CACHE_CHUNK_BYTES * 2 + 17];
        assert_eq!(client.get(&key).await.unwrap(), None);
        client.put(&key, &body).await.unwrap();
        assert_eq!(client.stat(&key).await.unwrap(), Some(body.len() as u64));
        assert_eq!(client.get(&key).await.unwrap(), Some(body));
        client.delete(&key).await.unwrap();
        assert_eq!(client.get(&key).await.unwrap(), None);
        drop(client);
        route_task.abort();
        home_task.abort();
        inbound_task.abort();
    }

    #[tokio::test]
    async fn loopback_webdav_sidecar_serves_the_sccache_method_set() {
        let temp = tempfile::tempdir().unwrap();
        let store = HomeCacheStore::new_for_test(temp.path().join("cache"), 4 * 1024 * 1024);
        let task = format!("task-test-{}", uuid::Uuid::new_v4());
        let job = format!("remote-test-{}", uuid::Uuid::new_v4());
        let token = uuid::Uuid::new_v4().simple().to_string();
        let mut home = HomeCacheSession::register(&task, &job, &token, store).unwrap();
        let (home_to_worker_tx, mut home_to_worker_rx) = mpsc::channel::<String>(64);
        let (worker_to_home_tx, mut worker_to_home_rx) = mpsc::channel::<String>(64);
        let worker = WorkerCacheRelay::default();
        let worker_inbound = worker.clone();
        let inbound_task = tokio::spawn(async move {
            while let Some(text) = home_to_worker_rx.recv().await {
                let frame: Value = serde_json::from_str(&text).unwrap();
                worker_inbound.serve_frame(&frame).await;
            }
        });
        let route_task_id = task.clone();
        let route_task = tokio::spawn(async move {
            while let Some(text) = worker_to_home_rx.recv().await {
                assert!(route_worker_frame(&route_task_id, &text));
            }
        });
        let home_task = tokio::spawn(async move {
            while let Some(frame) = home.next_frame().await {
                home.handle_frame(frame, &home_to_worker_tx).await;
            }
        });
        let relay_client = worker.client(job, token, format!("cloud:{task}"), worker_to_home_tx);
        let sidecar = WorkerCacheSidecar::start(relay_client).await.unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let root = format!("{}/", sidecar.endpoint());
        let propfind = Method::from_bytes(b"PROPFIND").unwrap();
        let response = http.request(propfind.clone(), &root).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::MULTI_STATUS);
        let root_xml = response.text().await.unwrap();
        assert!(root_xml.contains("<D:collection/>"));
        assert!(root_xml.contains("<D:getlastmodified>"));

        let key = cache_key('d');
        let object_url = format!("{}/{key}", sidecar.endpoint());
        assert_eq!(
            http.get(&object_url).send().await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
        let body = vec![0xa5; CACHE_CHUNK_BYTES + 31];
        assert_eq!(
            http.put(&object_url)
                .body(body.clone())
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::CREATED
        );
        let response = http.get(&object_url).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.bytes().await.unwrap().as_ref(), body.as_slice());
        let response = http.request(propfind, &object_url).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::MULTI_STATUS);
        let object_xml = response.text().await.unwrap();
        assert!(object_xml.contains(&format!(
            "<D:getcontentlength>{}</D:getcontentlength>",
            body.len()
        )));
        assert!(object_xml.contains("<D:resourcetype/>"));

        drop(sidecar);
        route_task.abort();
        home_task.abort();
        inbound_task.abort();
    }

    #[tokio::test]
    async fn wrong_relay_capability_never_enters_the_home_session() {
        let temp = tempfile::tempdir().unwrap();
        let task = format!("task-test-{}", uuid::Uuid::new_v4());
        let job = format!("remote-test-{}", uuid::Uuid::new_v4());
        let token = uuid::Uuid::new_v4().simple().to_string();
        let store = HomeCacheStore::new_for_test(temp.path().join("cache"), 1024 * 1024);
        let mut home = HomeCacheSession::register(&task, &job, &token, store).unwrap();
        let frame = json!({
            "t":REMOTE_CACHE_REQUEST_KIND,
            "id":job,
            "relay_token":uuid::Uuid::new_v4().simple().to_string(),
            "request_id":uuid::Uuid::new_v4().simple().to_string(),
            "op":"stat",
            "key":cache_key('c'),
        });
        assert!(route_worker_frame(&task, &frame.to_string()));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), home.next_frame())
                .await
                .is_err()
        );
    }
}
