//! Filesystem fingerprints and the session-list cache tier: row/codex/
//! baseline/intendant caches, the persisted session index, and its preload.

use super::*;

pub(crate) fn collect_files(root: &Path, suffix: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, suffix, out);
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(suffix))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
}

pub(crate) fn file_mtime_secs(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|m| metadata_mtime_secs(&m))
        .unwrap_or(0)
}

/// Last-ACTIVITY mtime for an intendant session log dir: the transcript
/// (`session.jsonl`) when present, else the dir itself. Daemon bookkeeping
/// (fission-ledger and meta rewrites land via `atomic_write`'s rename into
/// the dir) bumps the DIR mtime, which made month-old sessions sort — and
/// read — as "changed today" after every boot sweep. The transcript only
/// moves on real appends.
pub(crate) fn session_activity_mtime_secs(dir: &Path) -> u64 {
    match file_mtime_secs(&dir.join("session.jsonl")) {
        0 => file_mtime_secs(dir),
        secs => secs,
    }
}

pub(crate) fn metadata_mtime_secs(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn metadata_mtime_nanos(metadata: &std::fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub(crate) fn metadata_ctime_nanos(metadata: &std::fs::Metadata) -> i128 {
    crate::platform::metadata_ctime_nanos(metadata)
}

pub(crate) fn session_list_path_key(path: &Path) -> String {
    let normalized = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    normalized.to_string_lossy().to_string()
}

pub(crate) fn file_dependency_fingerprint(path: &Path) -> String {
    let path_key = session_list_path_key(path);
    match std::fs::metadata(path) {
        Ok(metadata) => {
            let (dev, ino) = crate::platform::metadata_dev_ino(&metadata);
            format!(
                "{path_key}\0{}\0{}\0{}\0{}\0{}",
                metadata.len(),
                metadata_mtime_nanos(&metadata),
                metadata_ctime_nanos(&metadata),
                dev,
                ino
            )
        }
        Err(_) => format!("{path_key}\0missing"),
    }
}

pub(crate) fn session_list_cache_key(
    namespace: &'static str,
    path: &Path,
    extra: impl Into<String>,
) -> Option<SessionListCacheKey> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let (dev, ino) = crate::platform::metadata_dev_ino(&metadata);
    Some(SessionListCacheKey {
        namespace,
        path: session_list_path_key(path),
        len: metadata.len(),
        mtime_nanos: metadata_mtime_nanos(&metadata),
        ctime_nanos: metadata_ctime_nanos(&metadata),
        dev,
        ino,
        extra: extra.into(),
    })
}

pub(crate) fn session_list_cache_slot(key: &SessionListCacheKey) -> String {
    format!("{}\0{}\0{}", key.namespace, key.path, key.extra)
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedSessionCacheKey {
    namespace: String,
    path: String,
    len: u64,
    #[serde(with = "string_u128")]
    mtime_nanos: u128,
    #[serde(with = "string_i128")]
    ctime_nanos: i128,
    dev: u64,
    ino: u64,
    extra: String,
}

impl PersistedSessionCacheKey {
    pub(crate) fn of(key: &SessionListCacheKey) -> Self {
        Self {
            namespace: key.namespace.to_string(),
            path: key.path.clone(),
            len: key.len,
            mtime_nanos: key.mtime_nanos,
            ctime_nanos: key.ctime_nanos,
            dev: key.dev,
            ino: key.ino,
            extra: key.extra.clone(),
        }
    }

    pub(crate) fn matches(&self, key: &SessionListCacheKey) -> bool {
        self.namespace == key.namespace
            && self.path == key.path
            && self.len == key.len
            && self.mtime_nanos == key.mtime_nanos
            && self.ctime_nanos == key.ctime_nanos
            && self.dev == key.dev
            && self.ino == key.ino
            && self.extra == key.extra
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedSessionCacheEntry<T> {
    #[serde(default)]
    schema: u32,
    key: PersistedSessionCacheKey,
    value: T,
}

/// Schema stamp for a namespace's persisted entries. Old entries (schema 0
/// predates the field) mismatch after a bump and read as cache misses.
pub(crate) fn persisted_namespace_schema(namespace: &str) -> u32 {
    match namespace {
        // v1: summaries persist `first_usage_event` instead of the full
        // `usage_events` history; pre-v1 entries would deserialize with a
        // defaulted first event and mis-baseline forked sessions.
        "codex" => 1,
        _ => 0,
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct PersistedIntendantSessionEntry {
    fingerprint: SessionDirFingerprint,
    row: serde_json::Value,
}

pub(crate) fn session_index_dir() -> PathBuf {
    crate::platform::intendant_home()
        .join("cache")
        .join("session_index")
}

pub(crate) fn session_index_entry_path_in(base: &Path, namespace: &str, slot: &str) -> PathBuf {
    let digest = ring::digest::digest(&ring::digest::SHA256, slot.as_bytes());
    let mut name = String::with_capacity(digest.as_ref().len() * 2 + 5);
    for byte in digest.as_ref() {
        name.push_str(&format!("{byte:02x}"));
    }
    name.push_str(".json");
    base.join(namespace).join(name)
}

pub(crate) fn session_index_entry_path(namespace: &str, slot: &str) -> PathBuf {
    session_index_entry_path_in(&session_index_dir(), namespace, slot)
}

pub(crate) fn write_session_index_entry(path: &Path, body: &[u8]) {
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let _ = crate::file_watcher::atomic_write(path, body);
}

pub(crate) fn load_persisted_session_entry_in<T: serde::de::DeserializeOwned>(
    base: &Path,
    key: &SessionListCacheKey,
) -> Option<T> {
    let path = session_index_entry_path_in(base, key.namespace, &session_list_cache_slot(key));
    let bytes = std::fs::read(path).ok()?;
    let entry: PersistedSessionCacheEntry<T> = serde_json::from_slice(&bytes).ok()?;
    if entry.schema != persisted_namespace_schema(key.namespace) {
        return None;
    }
    entry.key.matches(key).then_some(entry.value)
}

/// The persisted (on-disk) tier of the session index is DISABLED in
/// unit-test builds, at these ambient wrappers: the index is the daemon's
/// own derived cache under its state root, so every row a test's catalog
/// scan parses would otherwise write a JSON blob into the machine's real
/// `~/.intendant/cache/session_index` — the write-through leak the
/// empty-HOME acceptance run catches (tests-are-hermetic). Same shape as
/// listener.rs's `#[cfg(not(test))]` warm-scan gate; the in-memory tiers
/// stay fully exercised and the `_in`-suffixed fns remain the persisted
/// tier's testable seam (see this file's round-trip tests).
pub(crate) fn load_persisted_session_entry<T: serde::de::DeserializeOwned>(
    key: &SessionListCacheKey,
) -> Option<T> {
    if cfg!(test) {
        return None;
    }
    load_persisted_session_entry_in(&session_index_dir(), key)
}

pub(crate) fn store_persisted_session_entry_in<T: Serialize>(
    base: &Path,
    key: &SessionListCacheKey,
    value: &T,
) {
    let entry = PersistedSessionCacheEntry {
        schema: persisted_namespace_schema(key.namespace),
        key: PersistedSessionCacheKey::of(key),
        value,
    };
    let Ok(body) = serde_json::to_vec(&entry) else {
        return;
    };
    let path = session_index_entry_path_in(base, key.namespace, &session_list_cache_slot(key));
    write_session_index_entry(&path, &body);
}

pub(crate) fn store_persisted_session_entry<T: Serialize>(key: &SessionListCacheKey, value: &T) {
    // Disk tier off under test — see load_persisted_session_entry.
    if cfg!(test) {
        return;
    }
    store_persisted_session_entry_in(&session_index_dir(), key, value);
}

pub(crate) fn load_persisted_intendant_row(
    fingerprint: &SessionDirFingerprint,
) -> Option<serde_json::Value> {
    // Disk tier off under test — see load_persisted_session_entry.
    if cfg!(test) {
        return None;
    }
    let path = session_index_entry_path("intendant-row", &fingerprint.path);
    let bytes = std::fs::read(path).ok()?;
    let entry: PersistedIntendantSessionEntry = serde_json::from_slice(&bytes).ok()?;
    (&entry.fingerprint == fingerprint).then_some(entry.row)
}

pub(crate) fn store_persisted_intendant_row(
    fingerprint: &SessionDirFingerprint,
    row: &serde_json::Value,
) {
    // Disk tier off under test — see load_persisted_session_entry.
    if cfg!(test) {
        return;
    }
    let entry = PersistedIntendantSessionEntry {
        fingerprint: fingerprint.clone(),
        row: row.clone(),
    };
    let Ok(body) = serde_json::to_vec(&entry) else {
        return;
    };
    let path = session_index_entry_path("intendant-row", &fingerprint.path);
    write_session_index_entry(&path, &body);
}

pub(crate) fn remove_persisted_intendant_row(dir: &Path) {
    // Disk tier off under test — see load_persisted_session_entry.
    if cfg!(test) {
        return;
    }
    let path = session_index_entry_path("intendant-row", &session_list_path_key(dir));
    let _ = std::fs::remove_file(path);
}

/// Bulk-load the on-disk session index into the in-memory caches once per
/// process. Thousands of lazy per-entry reads during the first list scan
/// cost seconds sequentially; one parallel sweep up front costs a fraction
/// of that. Entries land exactly as `store_*` would have put them — the
/// normal lookup path still validates every fingerprint against the live
/// filesystem, so a stale preloaded entry can never be served.
pub(crate) type PreloadApply = fn(&'static str, &[u8]) -> PreloadOutcome;

pub(crate) fn preload_session_index() {
    static PRELOADED: std::sync::Once = std::sync::Once::new();
    PRELOADED.call_once(|| {
        let base = session_index_dir();
        let namespaces: [(&'static str, PreloadApply); 4] = [
            ("codex", preload_codex_entry),
            ("claude-code", preload_row_entry),
            ("gemini", preload_row_entry),
            ("codex-parent-baseline", preload_baseline_entry),
        ];
        std::thread::scope(|scope| {
            for (namespace, apply) in namespaces {
                let dir = base.join(namespace);
                scope.spawn(move || preload_namespace_dir(&dir, namespace, apply));
            }
            let intendant_dir = base.join("intendant-row");
            scope.spawn(move || {
                preload_namespace_dir(&intendant_dir, "intendant-row", preload_intendant_entry)
            });
        });
    });
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PreloadOutcome {
    /// Entry landed in the in-memory cache.
    Loaded,
    /// The session file/dir the entry indexes is gone — the entry is dead
    /// weight and its index file should be deleted.
    TargetMissing,
    /// Not loadable by this build (schema mismatch, unreadable JSON). Keep
    /// the file: an older or newer daemon sharing this HOME may still own
    /// it, and refreshes overwrite the same slot anyway.
    Skipped,
}

pub(crate) fn preload_namespace_dir(dir: &Path, namespace: &'static str, apply: PreloadApply) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(".tmp.")
            || (name.starts_with(".intendant-write-") && name.ends_with(".tmp"))
        {
            // Writers rename these away within the same call; anything
            // older than a minute is litter from a crashed daemon.
            let aged = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|age| age > std::time::Duration::from_secs(60))
                .unwrap_or(false);
            if aged {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    if paths.is_empty() {
        return;
    }
    // The largest namespace holds thousands of entries; a few reader
    // threads keep the preload in the hundreds of milliseconds.
    let chunk = paths.len().div_ceil(4).max(1);
    std::thread::scope(|scope| {
        for slice in paths.chunks(chunk) {
            scope.spawn(move || {
                for path in slice {
                    if let Ok(bytes) = std::fs::read(path) {
                        if apply(namespace, &bytes) == PreloadOutcome::TargetMissing {
                            let _ = std::fs::remove_file(path);
                        }
                    }
                }
            });
        }
    });
}

pub(crate) fn runtime_session_cache_key(
    namespace: &'static str,
    key: PersistedSessionCacheKey,
) -> Option<SessionListCacheKey> {
    if key.namespace != namespace {
        return None;
    }
    Some(SessionListCacheKey {
        namespace,
        path: key.path,
        len: key.len,
        mtime_nanos: key.mtime_nanos,
        ctime_nanos: key.ctime_nanos,
        dev: key.dev,
        ino: key.ino,
        extra: key.extra,
    })
}

/// Lenient fallback for entries this build cannot parse (legacy or future
/// shapes): both formats keep the indexed session's path under `key.path`
/// (generic namespaces) or `fingerprint.path` (intendant rows), so a dead
/// target is still detectable — and prunable — without understanding the
/// rest of the entry. Anything else unreadable is left alone.
pub(crate) fn preload_unparsed_entry_outcome(bytes: &[u8]) -> PreloadOutcome {
    #[derive(Deserialize)]
    struct ProbePath {
        path: Option<String>,
    }
    #[derive(Deserialize)]
    struct Probe {
        key: Option<ProbePath>,
        fingerprint: Option<ProbePath>,
    }
    let Ok(probe) = serde_json::from_slice::<Probe>(bytes) else {
        return PreloadOutcome::Skipped;
    };
    let path = probe
        .key
        .and_then(|key| key.path)
        .or_else(|| probe.fingerprint.and_then(|fingerprint| fingerprint.path));
    match path {
        Some(path) if !path.is_empty() && !Path::new(&path).exists() => {
            PreloadOutcome::TargetMissing
        }
        _ => PreloadOutcome::Skipped,
    }
}

pub(crate) fn preload_row_entry(namespace: &'static str, bytes: &[u8]) -> PreloadOutcome {
    let Ok(entry) = serde_json::from_slice::<PersistedSessionCacheEntry<serde_json::Value>>(bytes)
    else {
        return preload_unparsed_entry_outcome(bytes);
    };
    let schema_matches = entry.schema == persisted_namespace_schema(namespace);
    let Some(key) = runtime_session_cache_key(namespace, entry.key) else {
        return PreloadOutcome::Skipped;
    };
    if !Path::new(&key.path).exists() {
        return PreloadOutcome::TargetMissing;
    }
    if !schema_matches {
        return PreloadOutcome::Skipped;
    }
    let slot = session_list_cache_slot(&key);
    let mut cache = session_list_row_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.entry(slot).or_insert(SessionListRowCacheEntry {
        key,
        row: entry.value,
    });
    PreloadOutcome::Loaded
}

pub(crate) fn preload_codex_entry(namespace: &'static str, bytes: &[u8]) -> PreloadOutcome {
    let Ok(entry) =
        serde_json::from_slice::<PersistedSessionCacheEntry<CodexSessionListSummary>>(bytes)
    else {
        return preload_unparsed_entry_outcome(bytes);
    };
    let schema_matches = entry.schema == persisted_namespace_schema(namespace);
    let Some(key) = runtime_session_cache_key(namespace, entry.key) else {
        return PreloadOutcome::Skipped;
    };
    if !Path::new(&key.path).exists() {
        return PreloadOutcome::TargetMissing;
    }
    if !schema_matches {
        return PreloadOutcome::Skipped;
    }
    let slot = session_list_cache_slot(&key);
    let mut cache = codex_session_list_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.entry(slot).or_insert(CodexSessionListCacheEntry {
        key,
        summary: entry.value,
    });
    PreloadOutcome::Loaded
}

pub(crate) fn preload_baseline_entry(namespace: &'static str, bytes: &[u8]) -> PreloadOutcome {
    let Ok(entry) =
        serde_json::from_slice::<PersistedSessionCacheEntry<Option<SessionUsage>>>(bytes)
    else {
        return preload_unparsed_entry_outcome(bytes);
    };
    let schema_matches = entry.schema == persisted_namespace_schema(namespace);
    let Some(key) = runtime_session_cache_key(namespace, entry.key) else {
        return PreloadOutcome::Skipped;
    };
    if !Path::new(&key.path).exists() {
        return PreloadOutcome::TargetMissing;
    }
    if !schema_matches {
        return PreloadOutcome::Skipped;
    }
    let slot = session_list_cache_slot(&key);
    let mut cache = codex_parent_usage_baseline_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache
        .entry(slot)
        .or_insert(CodexParentUsageBaselineCacheEntry {
            key,
            usage: entry.value,
        });
    PreloadOutcome::Loaded
}

pub(crate) fn preload_intendant_entry(_namespace: &'static str, bytes: &[u8]) -> PreloadOutcome {
    let Ok(entry) = serde_json::from_slice::<PersistedIntendantSessionEntry>(bytes) else {
        return preload_unparsed_entry_outcome(bytes);
    };
    if !Path::new(&entry.fingerprint.path).exists() {
        return PreloadOutcome::TargetMissing;
    }
    let slot = entry.fingerprint.path.clone();
    let mut cache = intendant_session_list_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    cache.entry(slot).or_insert(IntendantSessionListCacheEntry {
        fingerprint: entry.fingerprint,
        row: entry.row,
    });
    PreloadOutcome::Loaded
}

pub(crate) fn session_list_row_cache() -> &'static Mutex<HashMap<String, SessionListRowCacheEntry>>
{
    SESSION_LIST_ROW_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn cached_session_list_row(key: &SessionListCacheKey) -> Option<serde_json::Value> {
    let slot = session_list_cache_slot(key);
    {
        let cache = session_list_row_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(&slot).filter(|entry| &entry.key == key) {
            return Some(entry.row.clone());
        }
    }
    // Miss in memory (fresh process): try the on-disk index before paying
    // a full re-parse. A hit re-seeds the in-memory tier.
    let row = load_persisted_session_entry::<serde_json::Value>(key)?;
    let mut cache = session_list_row_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(
        slot,
        SessionListRowCacheEntry {
            key: key.clone(),
            row: row.clone(),
        },
    );
    Some(row)
}

pub(crate) fn store_session_list_row(key: SessionListCacheKey, row: &serde_json::Value) {
    store_persisted_session_entry(&key, row);
    let slot = session_list_cache_slot(&key);
    let mut cache = session_list_row_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(
        slot,
        SessionListRowCacheEntry {
            key,
            row: row.clone(),
        },
    );
}

pub(crate) fn codex_session_list_cache(
) -> &'static Mutex<HashMap<String, CodexSessionListCacheEntry>> {
    CODEX_SESSION_LIST_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn cached_codex_session_list_entry(
    key: &SessionListCacheKey,
) -> Option<CodexSessionListCacheEntry> {
    let slot = session_list_cache_slot(key);
    {
        let cache = codex_session_list_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(&slot).filter(|entry| &entry.key == key) {
            return Some(entry.clone());
        }
    }
    let summary = load_persisted_session_entry::<CodexSessionListSummary>(key)?;
    let entry = CodexSessionListCacheEntry {
        key: key.clone(),
        summary,
    };
    let mut cache = codex_session_list_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(slot, entry.clone());
    Some(entry)
}

pub(crate) fn store_codex_session_list_entry(
    key: SessionListCacheKey,
    summary: CodexSessionListSummary,
) {
    store_persisted_session_entry(&key, &summary);
    let slot = session_list_cache_slot(&key);
    let mut cache = codex_session_list_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(slot, CodexSessionListCacheEntry { key, summary });
}

pub(crate) fn codex_parent_usage_baseline_cache(
) -> &'static Mutex<HashMap<String, CodexParentUsageBaselineCacheEntry>> {
    CODEX_PARENT_USAGE_BASELINE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn cached_codex_parent_usage_baseline(
    key: &SessionListCacheKey,
) -> Option<Option<SessionUsage>> {
    let slot = session_list_cache_slot(key);
    {
        let cache = codex_parent_usage_baseline_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(&slot).filter(|entry| &entry.key == key) {
            return Some(entry.usage);
        }
    }
    let usage = load_persisted_session_entry::<Option<SessionUsage>>(key)?;
    let mut cache = codex_parent_usage_baseline_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(
        slot,
        CodexParentUsageBaselineCacheEntry {
            key: key.clone(),
            usage,
        },
    );
    Some(usage)
}

pub(crate) fn store_codex_parent_usage_baseline(
    key: SessionListCacheKey,
    usage: Option<SessionUsage>,
) {
    store_persisted_session_entry(&key, &usage);
    let slot = session_list_cache_slot(&key);
    let mut cache = codex_parent_usage_baseline_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(slot, CodexParentUsageBaselineCacheEntry { key, usage });
}

pub(crate) fn intendant_session_list_cache(
) -> &'static Mutex<HashMap<String, IntendantSessionListCacheEntry>> {
    INTENDANT_SESSION_LIST_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn cached_intendant_session_list_row(
    fingerprint: &SessionDirFingerprint,
) -> Option<serde_json::Value> {
    {
        let cache = intendant_session_list_cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache
            .get(&fingerprint.path)
            .filter(|entry| &entry.fingerprint == fingerprint)
        {
            return Some(entry.row.clone());
        }
    }
    let row = load_persisted_intendant_row(fingerprint)?;
    let slot = fingerprint.path.clone();
    let mut cache = intendant_session_list_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(
        slot,
        IntendantSessionListCacheEntry {
            fingerprint: fingerprint.clone(),
            row: row.clone(),
        },
    );
    Some(row)
}

pub(crate) fn store_intendant_session_list_row(
    fingerprint: SessionDirFingerprint,
    row: &serde_json::Value,
) {
    store_persisted_intendant_row(&fingerprint, row);
    let slot = fingerprint.path.clone();
    let mut cache = intendant_session_list_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if cache.len() >= SESSION_LIST_ROW_CACHE_LIMIT && !cache.contains_key(&slot) {
        cache.clear();
    }
    cache.insert(
        slot,
        IntendantSessionListCacheEntry {
            fingerprint,
            row: row.clone(),
        },
    );
}

pub(crate) fn collect_recent_files(root: &Path, suffix: &str, limit: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect_files(root, suffix, &mut files);
    let mut seen = HashSet::new();
    files.retain(|path| {
        std::fs::canonicalize(path)
            .map(|canonical| seen.insert(canonical))
            .unwrap_or(true)
    });
    files.sort_by_key(|b| std::cmp::Reverse(file_mtime_secs(b)));
    files.truncate(limit);
    files
}

pub(crate) fn derive_project_root_from_cwd(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd?.trim();
    if cwd.is_empty() {
        return None;
    }

    let mut current = PathBuf::from(cwd);
    if !current.is_absolute() {
        return Some(cwd.to_string());
    }
    if current.is_file() {
        current.pop();
    }

    loop {
        if current.join(".git").exists() {
            return Some(current.to_string_lossy().to_string());
        }
        if !current.pop() {
            break;
        }
    }

    Some(cwd.to_string())
}

pub(crate) fn read_text_head_tail(path: &Path, head_bytes: u64, tail_bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok().map(|m| m.len()).unwrap_or(0);
    if len <= head_bytes.saturating_add(tail_bytes) {
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).ok()?;
        return Some(String::from_utf8_lossy(&buf).to_string());
    }

    let mut head = vec![0; head_bytes as usize];
    let head_len = file.read(&mut head).ok()?;
    head.truncate(head_len);

    file.seek(SeekFrom::End(-(tail_bytes as i64))).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;

    let mut out = String::from_utf8_lossy(&head).to_string();
    out.push('\n');
    out.push_str(&String::from_utf8_lossy(&tail));
    Some(out)
}

pub(crate) fn read_text_tail(path: &Path, tail_bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok().map(|m| m.len()).unwrap_or(0);
    if len <= tail_bytes {
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).ok()?;
        return Some(String::from_utf8_lossy(&buf).to_string());
    }

    file.seek(SeekFrom::End(-(tail_bytes as i64))).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    Some(String::from_utf8_lossy(&tail).to_string())
}

pub(crate) fn file_mtime_string(path: &Path) -> Option<String> {
    mtime_secs_to_string(file_mtime_secs(path))
}

pub(crate) fn mtime_secs_to_string(secs: u64) -> Option<String> {
    if secs == 0 {
        return None;
    }
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
    let dt: chrono::DateTime<chrono::Local> = t.into();
    Some(dt.format("%Y-%m-%d %H:%M:%S").to_string())
}

pub(crate) fn file_size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web_gateway::session_catalog::rows_usage::tests::total_usage;

    fn persisted_test_key(extra: &str) -> SessionListCacheKey {
        SessionListCacheKey {
            namespace: "test-rows",
            path: "/tmp/example/session.jsonl".to_string(),
            len: 1234,
            mtime_nanos: 111_222_333_444_555_666_777,
            ctime_nanos: -42,
            dev: 7,
            ino: 99,
            extra: extra.to_string(),
        }
    }

    #[test]
    fn persisted_session_entry_round_trips_and_validates_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let key = persisted_test_key("v1");
        let row = serde_json::json!({"session_id": "s-1", "total_tokens": 42});

        assert!(load_persisted_session_entry_in::<serde_json::Value>(dir.path(), &key).is_none());
        store_persisted_session_entry_in(dir.path(), &key, &row);
        assert_eq!(
            load_persisted_session_entry_in::<serde_json::Value>(dir.path(), &key),
            Some(row.clone())
        );

        // Any fingerprint drift (here: file length) must invalidate the entry.
        let mut stale = key.clone();
        stale.len += 1;
        assert!(load_persisted_session_entry_in::<serde_json::Value>(dir.path(), &stale).is_none());
        // A different `extra` is a different slot entirely.
        let other = persisted_test_key("v2");
        assert!(load_persisted_session_entry_in::<serde_json::Value>(dir.path(), &other).is_none());
    }

    #[test]
    fn persisted_session_entry_survives_128_bit_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let mut key = persisted_test_key("wide");
        key.mtime_nanos = u128::MAX;
        key.ctime_nanos = i128::MIN;
        let usage: Option<SessionUsage> = Some(SessionUsage {
            total_tokens: 10,
            prompt_tokens: 6,
            completion_tokens: 4,
            cache_creation_tokens: 0,
            cached_tokens: 2,
        });
        store_persisted_session_entry_in(dir.path(), &key, &usage);
        assert_eq!(
            load_persisted_session_entry_in::<Option<SessionUsage>>(dir.path(), &key),
            Some(usage)
        );
    }

    #[test]
    fn persisted_entry_rejects_corrupt_body() {
        let dir = tempfile::tempdir().unwrap();
        let key = persisted_test_key("corrupt");
        let path =
            session_index_entry_path_in(dir.path(), key.namespace, &session_list_cache_slot(&key));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        assert!(load_persisted_session_entry_in::<serde_json::Value>(dir.path(), &key).is_none());
    }

    /// Pre-schema "codex" entries carried the full usage_events history and
    /// no schema stamp; they must read as misses (a defaulted
    /// first_usage_event would mis-baseline forked sessions), while
    /// current-schema entries round-trip.
    #[test]
    fn persisted_codex_entry_schema_mismatch_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let mut key = persisted_test_key("schema");
        key.namespace = "codex";

        let legacy = serde_json::json!({
            "key": {
                "namespace": key.namespace,
                "path": key.path,
                "len": key.len,
                "mtime_nanos": key.mtime_nanos.to_string(),
                "ctime_nanos": key.ctime_nanos.to_string(),
                "dev": key.dev,
                "ino": key.ino,
                "extra": key.extra,
            },
            "value": {
                "id": "codex-1",
                "created_at": null,
                "session_cwd": null,
                "effective_cwd": null,
                "model": null,
                "lineage": {},
                "provider": "Codex",
                "usage": total_usage(10),
                "usage_events": [{"timestamp": null, "usage": total_usage(10)}],
                "daily_usage": {},
                "goal": null,
                "task": null,
                "turns": 1,
                "file_updated_at": null,
                "bytes": 5,
            },
        });
        let path =
            session_index_entry_path_in(dir.path(), key.namespace, &session_list_cache_slot(&key));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(
            load_persisted_session_entry_in::<CodexSessionListSummary>(dir.path(), &key).is_none()
        );

        // A freshly stored entry (current schema) round-trips.
        let summary = CodexSessionListSummary {
            id: "codex-1".to_string(),
            created_at: None,
            session_cwd: None,
            effective_cwd: None,
            model: None,
            lineage: SessionLineageMetadata::default(),
            provider: Some("Codex".to_string()),
            usage: total_usage(10),
            first_usage_event: Some(CodexUsageEvent {
                timestamp: None,
                usage: total_usage(10),
            }),
            daily_usage: BTreeMap::new(),
            goal: None,
            task: None,
            turns: 1,
            file_updated_at: None,
            bytes: 5,
            preview: None,
        };
        store_persisted_session_entry_in(dir.path(), &key, &summary);
        let loaded = load_persisted_session_entry_in::<CodexSessionListSummary>(dir.path(), &key)
            .expect("current-schema entry loads");
        assert_eq!(loaded.id, "codex-1");
        assert_eq!(
            loaded.first_usage_event.map(|event| event.usage),
            Some(total_usage(10))
        );
    }

    #[test]
    fn preload_prunes_entries_for_deleted_targets() {
        let base = tempfile::tempdir().unwrap();
        let target_dir = tempfile::tempdir().unwrap();
        let live_target = target_dir.path().join("live.jsonl");
        std::fs::write(&live_target, b"{}\n").unwrap();

        let entry_for = |path: &Path, extra: &str| -> (PathBuf, Vec<u8>) {
            let key = SessionListCacheKey {
                namespace: "claude-code",
                path: path.to_string_lossy().to_string(),
                len: 2,
                mtime_nanos: 1,
                ctime_nanos: 1,
                dev: 1,
                ino: 1,
                extra: extra.to_string(),
            };
            let entry = PersistedSessionCacheEntry {
                schema: persisted_namespace_schema(key.namespace),
                key: PersistedSessionCacheKey::of(&key),
                value: serde_json::json!({"session_id": extra}),
            };
            let file = session_index_entry_path_in(
                base.path(),
                key.namespace,
                &session_list_cache_slot(&key),
            );
            (file, serde_json::to_vec(&entry).unwrap())
        };

        let (live_file, live_bytes) = entry_for(&live_target, "live");
        let missing_target = target_dir.path().join("deleted.jsonl");
        let (dead_file, dead_bytes) = entry_for(&missing_target, "dead");
        std::fs::create_dir_all(live_file.parent().unwrap()).unwrap();
        std::fs::write(&live_file, &live_bytes).unwrap();
        std::fs::write(&dead_file, &dead_bytes).unwrap();

        preload_namespace_dir(
            &base.path().join("claude-code"),
            "claude-code",
            preload_row_entry,
        );

        assert!(live_file.exists(), "entry for a live session is kept");
        assert!(!dead_file.exists(), "entry for a deleted session is pruned");

        // Outcome-level checks: schema drift is skipped (kept on disk for
        // whichever daemon owns it), a missing target reports prunable.
        assert_eq!(
            preload_row_entry("claude-code", &dead_bytes),
            PreloadOutcome::TargetMissing
        );
        let mut wrong_schema: serde_json::Value = serde_json::from_slice(&live_bytes).unwrap();
        wrong_schema["schema"] = serde_json::json!(99);
        assert_eq!(
            preload_row_entry("claude-code", &serde_json::to_vec(&wrong_schema).unwrap()),
            PreloadOutcome::Skipped
        );

        // Legacy-shape entries no build of this daemon can parse are still
        // prunable through the path probe once their session is gone, and
        // kept while it is alive.
        let legacy = |target: &Path| {
            serde_json::to_vec(&serde_json::json!({
                "fingerprint": {"path": target.to_string_lossy(), "entries": []},
                "row": {"session_id": "legacy"},
            }))
            .unwrap()
        };
        assert_eq!(
            preload_intendant_entry("intendant-row", &legacy(&missing_target)),
            PreloadOutcome::TargetMissing
        );
        assert_eq!(
            preload_intendant_entry("intendant-row", &legacy(&live_target)),
            PreloadOutcome::Skipped
        );
    }
}
