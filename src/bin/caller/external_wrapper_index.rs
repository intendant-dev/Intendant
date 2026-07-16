use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

const INDEX_FILE: &str = "external_wrapper_index.json";
const INDEX_VERSION: u32 = 1;

static INDEX_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

// Session-list scans consult this index once per external row; re-reading
// and re-parsing the whole file each time made listing quadratic in the
// session count. The parsed index is cached per path and revalidated by
// file length + mtime, so cross-process writers are still picked up.
// Lock order is always INDEX_LOCK -> INDEX_CACHE.
static INDEX_CACHE: OnceLock<Mutex<HashMap<PathBuf, CachedWrapperIndex>>> = OnceLock::new();

struct CachedWrapperIndex {
    len: u64,
    mtime_nanos: u128,
    index: ExternalWrapperIndex,
}

fn index_file_fingerprint(path: &Path) -> (u64, u128) {
    match fs::metadata(path) {
        Ok(meta) => (
            meta.len(),
            meta.modified()
                .ok()
                .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ),
        Err(_) => (0, 0),
    }
}

fn index_cache() -> &'static Mutex<HashMap<PathBuf, CachedWrapperIndex>> {
    INDEX_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Run `f` against the current index without cloning it. Callers must hold
/// INDEX_LOCK.
fn with_index_unlocked<R>(home: &Path, f: impl FnOnce(&ExternalWrapperIndex) -> R) -> R {
    let path = index_path(home);
    let (len, mtime_nanos) = index_file_fingerprint(&path);
    let mut cache = index_cache().lock().unwrap_or_else(|e| e.into_inner());
    let cached_valid = cache
        .get(&path)
        .is_some_and(|entry| entry.len == len && entry.mtime_nanos == mtime_nanos);
    if !cached_valid {
        let index = read_index_from_disk(&path);
        cache.insert(
            path.clone(),
            CachedWrapperIndex {
                len,
                mtime_nanos,
                index,
            },
        );
    }
    f(&cache
        .get(&path)
        .expect("wrapper index cache entry just ensured")
        .index)
}

fn note_index_written(home: &Path, index: &ExternalWrapperIndex) {
    let path = index_path(home);
    let (len, mtime_nanos) = index_file_fingerprint(&path);
    let mut cache = index_cache().lock().unwrap_or_else(|e| e.into_inner());
    cache.insert(
        path,
        CachedWrapperIndex {
            len,
            mtime_nanos,
            index: index.clone(),
        },
    );
}

/// Lifecycle state of a wrapper record. `Superseded` rows lost an identity
/// conflict (see the demotion loops in [`upsert`]) and are retained for
/// history; the preference order ([`active_wrapper_in`]) puts `Active` rows
/// first. Index files written before this field existed — and files
/// rewritten by older binaries, which drop fields they don't know — carry
/// no `state`: those rows deserialize as `Active` and their demotions are
/// still expressed by the legacy `updated_at_secs == 0` sentinel, which the
/// preference order's timestamp tie-break honors.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum WrapperState {
    #[default]
    Active,
    Superseded,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExternalWrapperRecord {
    pub source: String,
    pub backend_session_id: String,
    pub intendant_session_id: String,
    pub log_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,
    /// Activity mtime of the wrapper's log dir — EXCEPT that `0` is the
    /// deliberate supersession sentinel: `upsert` demotes rows that lost an
    /// identity conflict by zeroing this field (see the demotion loops
    /// there), which sorts them behind every live row. Never "fix" a zero
    /// by re-stamping it with a fresh mtime — that resurrects a demoted
    /// row. [`WrapperState`] carries the same fact explicitly; the sentinel
    /// stays written for older readers.
    #[serde(default)]
    pub updated_at_secs: u64,
    /// Explicit lifecycle state (defaults to `Active` so pre-`state` index
    /// files parse unchanged). Kept in lockstep with the `updated_at_secs`
    /// zero-sentinel: demotion sets both, reactivation clears both.
    #[serde(default)]
    pub state: WrapperState,
    /// Resolved native backend log (e.g. the Codex rollout file) for this
    /// record's backend session, cached by [`record_rollout_path`] the
    /// first time a consumer pays for the full scan. Never trust it
    /// blindly: resolve through [`resolved_rollout_path`], which re-checks
    /// existence plus a caller-supplied identity predicate — native files
    /// migrate (Codex `sessions/` → `archived_sessions/`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_path: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ExternalWrapperIndex {
    version: u32,
    #[serde(default)]
    wrappers: Vec<ExternalWrapperRecord>,
}

impl Default for ExternalWrapperIndex {
    fn default() -> Self {
        Self {
            version: INDEX_VERSION,
            wrappers: Vec::new(),
        }
    }
}

pub fn index_path(home: &Path) -> PathBuf {
    crate::platform::intendant_home_in(home).join(INDEX_FILE)
}

pub fn home_from_log_dir(log_dir: &Path) -> Option<PathBuf> {
    // Process state root first: under `$INTENDANT_HOME` (or the unit-test
    // scratch root) the logs tree need not carry the `<home>/.intendant`
    // shape, but a log dir inside it still belongs to the process home —
    // callers feed the returned home back through `intendant_home_in`-based
    // paths (index_path, wrappers_for), which resolve to the same root.
    // With the default root this matches what the shape walk returns.
    if log_dir.parent() == Some(crate::platform::intendant_home().join("logs").as_path()) {
        return Some(crate::platform::home_dir());
    }
    let logs_dir = log_dir.parent()?;
    if logs_dir.file_name().and_then(|name| name.to_str()) != Some("logs") {
        return None;
    }
    let intendant_dir = logs_dir.parent()?;
    if intendant_dir.file_name().and_then(|name| name.to_str()) != Some(".intendant") {
        return None;
    }
    intendant_dir.parent().map(Path::to_path_buf)
}

pub fn upsert_from_log_dir(
    source: &str,
    backend_session_id: &str,
    intendant_session_id: &str,
    log_dir: &Path,
) -> Result<(), String> {
    let Some(home) = home_from_log_dir(log_dir) else {
        return Ok(());
    };
    upsert(
        &home,
        source,
        backend_session_id,
        intendant_session_id,
        log_dir,
        project_root_from_log_dir(log_dir).as_deref(),
    )
}

pub fn upsert(
    home: &Path,
    source: &str,
    backend_session_id: &str,
    intendant_session_id: &str,
    log_dir: &Path,
    project_root: Option<&Path>,
) -> Result<(), String> {
    let source = crate::session_names::normalize_source(source);
    let backend_session_id = backend_session_id.trim();
    let intendant_session_id = intendant_session_id.trim();
    let log_dir_session_id = log_dir_session_id(log_dir);
    let stored_intendant_session_id = log_dir_session_id
        .as_deref()
        .unwrap_or(intendant_session_id)
        .trim();
    if source.is_empty()
        || source == "intendant"
        || backend_session_id.is_empty()
        || intendant_session_id.is_empty()
        || stored_intendant_session_id.is_empty()
        || backend_session_id == stored_intendant_session_id
        || !crate::external_agent::source_session_id_is_canonical(&source, backend_session_id)
    {
        return Ok(());
    }
    // Attribution guard: the record is stored under the log dir's identity,
    // so the caller's wrapper id must actually name that dir (directly, or
    // via the meta-canonical id of a renamed dir). A mismatch means the
    // caller is holding someone else's log — e.g. an identity event the bus
    // tee copied into the daemon session's log — and indexing it would
    // re-attribute the wrapper binding to that log's session and demote the
    // real wrapper as an identity conflict.
    if intendant_session_id != stored_intendant_session_id
        && crate::session_identity::canonical_session_id_from_meta(log_dir).as_deref()
            != Some(intendant_session_id)
    {
        return Ok(());
    }

    let _guard = INDEX_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let log_path = log_dir.to_string_lossy().to_string();
    let updated_at_secs =
        file_mtime_secs(&log_dir.join("session.jsonl")).max(file_mtime_secs(log_dir));
    let project_root = project_root.map(|path| path.to_string_lossy().to_string());

    // The shared predicates between the read-only dirty pass and the
    // mutation pass below — keep them in one place so the two passes cannot
    // drift.
    let conflicts_backend = |record: &ExternalWrapperRecord| {
        record.source == source
            && record.backend_session_id == backend_session_id
            && record.intendant_session_id != stored_intendant_session_id
    };
    let conflicts_wrapper = |record: &ExternalWrapperRecord| {
        record.source == source
            && record.intendant_session_id == stored_intendant_session_id
            && record.backend_session_id != backend_session_id
    };
    let is_exact_identity = |record: &ExternalWrapperRecord| {
        record.source == source
            && record.backend_session_id == backend_session_id
            && record.intendant_session_id == stored_intendant_session_id
    };
    let needs_demotion = |record: &ExternalWrapperRecord| {
        record.updated_at_secs != 0 || record.state != WrapperState::Superseded
    };

    // Session-list scans upsert every external row on every pass; rewriting
    // the whole index per unchanged row made listing quadratic, and cloning
    // it per unchanged row still churned every record's strings. Decide
    // against the cached index in place and clone only when a write will
    // follow.
    let dirty = with_index_unlocked(home, |index| {
        let any_demotion = index.wrappers.iter().any(|record| {
            (conflicts_backend(record) || conflicts_wrapper(record)) && needs_demotion(record)
        });
        let upsert_changes = match index
            .wrappers
            .iter()
            .find(|record| is_exact_identity(record))
        {
            Some(existing) => {
                existing.log_path != log_path
                    || existing.project_root != project_root
                    || existing.updated_at_secs != updated_at_secs
                    || existing.state != WrapperState::Active
            }
            None => true,
        };
        any_demotion || upsert_changes
    });
    if !dirty {
        return Ok(());
    }

    let mut index = with_index_unlocked(home, |index| index.clone());

    // Demotion, not deletion: rows that conflict with the upserted identity
    // (same backend session under another wrapper; same wrapper now bound to
    // another backend session) are kept for history and marked superseded.
    // `updated_at_secs = 0` is the DELIBERATE legacy supersession sentinel —
    // it sorts demoted rows behind every live row for readers that predate
    // the explicit `state` field — so demotion always sets BOTH together.
    // Do not "repair" the zero with a real mtime; only a fresh upsert of the
    // row's exact identity triple (the branch below) may make it current
    // again.
    for record in index
        .wrappers
        .iter_mut()
        .filter(|record| conflicts_backend(record) || conflicts_wrapper(record))
    {
        record.updated_at_secs = 0;
        record.state = WrapperState::Superseded;
    }

    if let Some(existing) = index
        .wrappers
        .iter_mut()
        .find(|record| is_exact_identity(record))
    {
        existing.log_path = log_path;
        existing.project_root = project_root;
        // Reactivation: an upsert of this exact identity triple makes the
        // row current again — the mtime refresh already implied that under
        // the sentinel semantics; `state` follows in lockstep.
        existing.updated_at_secs = updated_at_secs;
        existing.state = WrapperState::Active;
    } else {
        index.wrappers.push(ExternalWrapperRecord {
            source,
            backend_session_id: backend_session_id.to_string(),
            intendant_session_id: stored_intendant_session_id.to_string(),
            log_path,
            project_root,
            updated_at_secs,
            state: WrapperState::Active,
            rollout_path: None,
        });
    }

    write_index_unlocked(home, &index)?;
    note_index_written(home, &index);
    Ok(())
}

/// The "active wins" preference order — the single definition every
/// consumer that picks THE wrapper for a backend session relies on:
/// explicit `state == Active` first, then the freshest `updated_at_secs`
/// (which also honors legacy zero-sentinel demotions on rows written
/// before `state` existed), then the lexically-greatest wrapper session id
/// as a deterministic tie-break. [`wrappers_for`] / [`wrappers_for_source`]
/// sort with this, so their first record is the preferred one.
fn wrapper_preference(a: &ExternalWrapperRecord, b: &ExternalWrapperRecord) -> std::cmp::Ordering {
    a.state
        .cmp(&b.state)
        .then_with(|| b.updated_at_secs.cmp(&a.updated_at_secs))
        .then_with(|| b.intendant_session_id.cmp(&a.intendant_session_id))
}

/// The preferred wrapper among `records` under the "active wins" order
/// ([`wrapper_preference`]). Use this instead of `records[0]` /
/// `.into_iter().next()` when selecting the wrapper for a backend session
/// from an already-fetched list, so the selection semantics live here.
pub fn active_wrapper_in(records: &[ExternalWrapperRecord]) -> Option<&ExternalWrapperRecord> {
    records.iter().min_by(|a, b| wrapper_preference(a, b))
}

/// The preferred wrapper record for `(source, backend_session_id)`:
/// `state == Active` preferred, latest `updated_at_secs` tie-break. Falls
/// back to the best superseded record when no active row survives
/// ([`wrappers_for`] drops records whose log dir is gone), so callers keep
/// resolving historical sessions.
pub fn active_wrapper_for(
    home: &Path,
    source: &str,
    backend_session_id: &str,
) -> Option<ExternalWrapperRecord> {
    wrappers_for(home, source, backend_session_id)
        .into_iter()
        .next()
}

pub fn wrappers_for(
    home: &Path,
    source: &str,
    backend_session_id: &str,
) -> Vec<ExternalWrapperRecord> {
    let source = crate::session_names::normalize_source(source);
    let backend_session_id = backend_session_id.trim();
    if source.is_empty() || backend_session_id.is_empty() {
        return Vec::new();
    }
    let _guard = INDEX_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut records: Vec<_> = with_index_unlocked(home, |index| {
        index
            .wrappers
            .iter()
            .filter_map(|record| {
                (record.source == source
                    && record.backend_session_id == backend_session_id
                    && Path::new(&record.log_path).is_dir())
                .then(|| normalize_log_identity(record.clone()))
                .flatten()
            })
            .collect()
    });
    records.sort_by(wrapper_preference);
    records
}

pub fn wrappers_for_source(home: &Path, source: &str) -> Vec<ExternalWrapperRecord> {
    let source = crate::session_names::normalize_source(source);
    if source.is_empty() {
        return Vec::new();
    }
    let _guard = INDEX_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut records: Vec<_> = with_index_unlocked(home, |index| {
        index
            .wrappers
            .iter()
            .filter_map(|record| {
                (record.source == source && Path::new(&record.log_path).is_dir())
                    .then(|| normalize_log_identity(record.clone()))
                    .flatten()
            })
            .collect()
    });
    records.sort_by(wrapper_preference);
    records
}

/// Remember the resolved native backend log (e.g. a Codex rollout file)
/// for `(source, backend_session_id)`. The path is stamped on EVERY record
/// of that backend session — the native file is a property of the backend
/// session, not of one wrapper — so resolution keeps working after the
/// current wrapper row is later demoted. No-op when the index has no
/// record for the session (the caller's scan fallback simply runs again
/// next time).
pub fn record_rollout_path(
    home: &Path,
    source: &str,
    backend_session_id: &str,
    rollout_path: &Path,
) -> Result<(), String> {
    let source = crate::session_names::normalize_source(source);
    let backend_session_id = backend_session_id.trim();
    let rollout_path = rollout_path.to_string_lossy().to_string();
    if source.is_empty() || backend_session_id.is_empty() || rollout_path.is_empty() {
        return Ok(());
    }
    let _guard = INDEX_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Decide against the cached index in place; clone only when a write
    // will follow (mirrors `upsert`).
    let dirty = with_index_unlocked(home, |index| {
        index.wrappers.iter().any(|record| {
            record.source == source
                && record.backend_session_id == backend_session_id
                && record.rollout_path.as_deref() != Some(rollout_path.as_str())
        })
    });
    if !dirty {
        return Ok(());
    }
    let mut index = with_index_unlocked(home, |index| index.clone());
    for record in index
        .wrappers
        .iter_mut()
        .filter(|record| record.source == source && record.backend_session_id == backend_session_id)
    {
        record.rollout_path = Some(rollout_path.clone());
    }
    write_index_unlocked(home, &index)?;
    note_index_written(home, &index);
    Ok(())
}

/// The stored native-log path for `(source, backend_session_id)`, verified
/// before trust: the file must still exist and `verify` (typically "does
/// its session id still match?") must accept it — otherwise `None`, and
/// the caller falls back to its scan (native files migrate, e.g. Codex
/// `sessions/` → `archived_sessions/`) and re-records the fresh location.
/// Stored candidates are tried in the "active wins" preference order;
/// `verify` runs outside the index lock (it may read a large file). Stale
/// paths are not cleared here — the caller's post-scan
/// [`record_rollout_path`] overwrite is the heal.
pub fn resolved_rollout_path(
    home: &Path,
    source: &str,
    backend_session_id: &str,
    verify: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let source = crate::session_names::normalize_source(source);
    let backend_session_id = backend_session_id.trim();
    if source.is_empty() || backend_session_id.is_empty() {
        return None;
    }
    let mut candidates: Vec<String> = {
        let _guard = INDEX_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        with_index_unlocked(home, |index| {
            let mut records: Vec<&ExternalWrapperRecord> = index
                .wrappers
                .iter()
                .filter(|record| {
                    record.source == source
                        && record.backend_session_id == backend_session_id
                        && record.rollout_path.is_some()
                })
                .collect();
            records.sort_by(|a, b| wrapper_preference(a, b));
            records
                .into_iter()
                .filter_map(|record| record.rollout_path.clone())
                .collect()
        })
    };
    let mut seen = std::collections::HashSet::new();
    candidates.retain(|candidate| seen.insert(candidate.clone()));
    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if path.is_file() && verify(&path) {
            return Some(path);
        }
    }
    None
}

pub fn record_to_json(record: &ExternalWrapperRecord) -> serde_json::Value {
    serde_json::json!({
        "source": record.source,
        "backend_session_id": record.backend_session_id,
        "intendant_session_id": record.intendant_session_id,
        "path": record.log_path,
        "project_root": record.project_root,
        "updated_at_secs": record.updated_at_secs,
        "state": record.state,
    })
}

fn read_index_from_disk(path: &Path) -> ExternalWrapperIndex {
    let Ok(contents) = fs::read_to_string(path) else {
        return ExternalWrapperIndex::default();
    };
    serde_json::from_str::<ExternalWrapperIndex>(&contents).unwrap_or_default()
}

fn write_index_unlocked(home: &Path, index: &ExternalWrapperIndex) -> Result<(), String> {
    let path = index_path(home);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create wrapper index dir: {e}"))?;
    }
    let tmp = path.with_extension("json.tmp");
    let body =
        serde_json::to_string_pretty(index).map_err(|e| format!("serialize wrapper index: {e}"))?;
    fs::write(&tmp, body).map_err(|e| format!("write wrapper index: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| format!("replace wrapper index: {e}"))
}

fn project_root_from_log_dir(log_dir: &Path) -> Option<PathBuf> {
    let meta = fs::read_to_string(log_dir.join("session_meta.json")).ok()?;
    serde_json::from_str::<crate::session_log::SessionMeta>(&meta)
        .ok()
        .and_then(|meta| meta.project_root)
        .map(PathBuf::from)
}

fn normalize_log_identity(mut record: ExternalWrapperRecord) -> Option<ExternalWrapperRecord> {
    record.intendant_session_id = log_dir_session_id(Path::new(&record.log_path))?;
    Some(record)
}

fn log_dir_session_id(log_dir: &Path) -> Option<String> {
    log_dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn file_mtime_secs(path: &Path) -> u64 {
    path.metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_demotes_stale_wrapper_for_same_backend_session() {
        let home = tempfile::tempdir().unwrap();
        let old_log_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join("e9532107-8c7f-4c1f-b88d-410d6d365505");
        let new_log_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join("ec5865e5-a5af-4b8c-81a1-545a3a6f8ba9");
        std::fs::create_dir_all(&old_log_dir).unwrap();
        std::fs::create_dir_all(&new_log_dir).unwrap();
        let backend_id = "019ea8b9-0000-7000-8000-000000000001";

        upsert(
            home.path(),
            "codex",
            backend_id,
            "e9532107-8c7f-4c1f-b88d-410d6d365505",
            &old_log_dir,
            None,
        )
        .unwrap();
        upsert(
            home.path(),
            "codex",
            backend_id,
            "ec5865e5-a5af-4b8c-81a1-545a3a6f8ba9",
            &new_log_dir,
            None,
        )
        .unwrap();

        let wrappers = wrappers_for(home.path(), "codex", backend_id);
        assert_eq!(wrappers.len(), 2);
        assert_eq!(
            wrappers[0].intendant_session_id,
            "ec5865e5-a5af-4b8c-81a1-545a3a6f8ba9"
        );
        assert_eq!(wrappers[0].log_path, new_log_dir.to_string_lossy());
        assert_eq!(wrappers[0].state, WrapperState::Active);
        assert_eq!(
            wrappers[1].intendant_session_id,
            "e9532107-8c7f-4c1f-b88d-410d6d365505"
        );
        // Demotion sets BOTH the legacy zero-sentinel (for older readers)
        // and the explicit state.
        assert_eq!(wrappers[1].updated_at_secs, 0);
        assert_eq!(wrappers[1].state, WrapperState::Superseded);

        let source_wrappers = wrappers_for_source(home.path(), "codex");
        assert_eq!(
            source_wrappers
                .first()
                .map(|record| record.intendant_session_id.as_str()),
            Some("ec5865e5-a5af-4b8c-81a1-545a3a6f8ba9")
        );

        // Pin the on-disk shape: the persisted JSON carries the zero
        // sentinel AND the explicit state, so both old and new readers see
        // the demotion.
        let raw = std::fs::read_to_string(index_path(home.path())).unwrap();
        let disk: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let demoted = disk["wrappers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["intendant_session_id"] == "e9532107-8c7f-4c1f-b88d-410d6d365505")
            .expect("demoted row persisted");
        assert_eq!(demoted["updated_at_secs"], 0);
        assert_eq!(demoted["state"], "superseded");
    }

    /// A caller holding someone else's log dir must not register that dir
    /// as the wrapper: the daemon session's log receives the bus tee's
    /// copies of every session's identity events, and indexing those made
    /// the daemon session the "active" wrapper of another session's thread
    /// (demoting the real wrapper as a conflict).
    #[test]
    fn upsert_skips_wrapper_id_that_does_not_name_the_log_dir() {
        let home = tempfile::tempdir().unwrap();
        let daemon_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join("11111111-aaaa-4aaa-8aaa-111111111111");
        std::fs::create_dir_all(&daemon_dir).unwrap();
        let backend_id = "019ea8b9-0000-7000-8000-0000000000f1";

        // Wrapper id names a DIFFERENT session than the dir: skipped.
        upsert(
            home.path(),
            "codex",
            backend_id,
            "22222222-bbbb-4bbb-8bbb-222222222222",
            &daemon_dir,
            None,
        )
        .unwrap();
        assert!(wrappers_for(home.path(), "codex", backend_id).is_empty());

        // The meta-canonical id of a renamed dir still counts as naming it.
        std::fs::write(
            daemon_dir.join("session_meta.json"),
            serde_json::json!({ "session_id": "33333333-cccc-4ccc-8ccc-333333333333" }).to_string(),
        )
        .unwrap();
        upsert(
            home.path(),
            "codex",
            backend_id,
            "33333333-cccc-4ccc-8ccc-333333333333",
            &daemon_dir,
            None,
        )
        .unwrap();
        let wrappers = wrappers_for(home.path(), "codex", backend_id);
        assert_eq!(wrappers.len(), 1);
        // Records store the dir's identity.
        assert_eq!(
            wrappers[0].intendant_session_id,
            "11111111-aaaa-4aaa-8aaa-111111111111"
        );
    }

    #[test]
    fn upsert_demotes_stale_backend_for_same_wrapper_session() {
        let home = tempfile::tempdir().unwrap();
        let log_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join("6036429e-54f9-4f93-b74d-04c060c79054");
        std::fs::create_dir_all(&log_dir).unwrap();
        let old_backend_id = "019ea99e-af1d-7c23-a57a-55a89c77f90b";
        let new_backend_id = "019ea9cc-76d0-7153-94cf-e98948d8ee8a";
        let wrapper_id = "6036429e-54f9-4f93-b74d-04c060c79054";

        upsert(
            home.path(),
            "codex",
            old_backend_id,
            wrapper_id,
            &log_dir,
            None,
        )
        .unwrap();
        upsert(
            home.path(),
            "codex",
            new_backend_id,
            wrapper_id,
            &log_dir,
            None,
        )
        .unwrap();

        let source_wrappers = wrappers_for_source(home.path(), "codex");
        assert_eq!(source_wrappers.len(), 2);
        assert_eq!(source_wrappers[0].backend_session_id, new_backend_id);
        assert_eq!(source_wrappers[0].intendant_session_id, wrapper_id);
        assert_eq!(source_wrappers[0].state, WrapperState::Active);
        assert_eq!(source_wrappers[1].backend_session_id, old_backend_id);
        assert_eq!(source_wrappers[1].updated_at_secs, 0);
        assert_eq!(source_wrappers[1].state, WrapperState::Superseded);
    }

    #[test]
    fn upsert_reactivates_previously_demoted_row() {
        let home = tempfile::tempdir().unwrap();
        let log_dir = home
            .path()
            .join(".intendant")
            .join("logs")
            .join("7b57f807-59a5-4bc5-9c2f-2f6b7f7f2a10");
        std::fs::create_dir_all(&log_dir).unwrap();
        let old_backend_id = "019ea99e-af1d-7c23-a57a-55a89c77f90b";
        let new_backend_id = "019ea9cc-76d0-7153-94cf-e98948d8ee8a";
        let wrapper_id = "7b57f807-59a5-4bc5-9c2f-2f6b7f7f2a10";

        upsert(
            home.path(),
            "codex",
            old_backend_id,
            wrapper_id,
            &log_dir,
            None,
        )
        .unwrap();
        upsert(
            home.path(),
            "codex",
            new_backend_id,
            wrapper_id,
            &log_dir,
            None,
        )
        .unwrap();
        // Re-upserting the demoted identity triple makes it current again —
        // sentinel and state move in lockstep in both directions.
        upsert(
            home.path(),
            "codex",
            old_backend_id,
            wrapper_id,
            &log_dir,
            None,
        )
        .unwrap();

        let source_wrappers = wrappers_for_source(home.path(), "codex");
        assert_eq!(source_wrappers.len(), 2);
        assert_eq!(source_wrappers[0].backend_session_id, old_backend_id);
        assert_eq!(source_wrappers[0].state, WrapperState::Active);
        assert!(source_wrappers[0].updated_at_secs > 0);
        assert_eq!(source_wrappers[1].backend_session_id, new_backend_id);
        assert_eq!(source_wrappers[1].state, WrapperState::Superseded);
        assert_eq!(source_wrappers[1].updated_at_secs, 0);
    }

    /// Pre-`state` index files (no `state`, no `rollout_path`) parse with
    /// every row active — the serde defaults are the compatibility
    /// contract for existing `external_wrapper_index.json` files.
    #[test]
    fn old_format_index_rows_parse_as_active() {
        let home = tempfile::tempdir().unwrap();
        let wrapper_id = "0d9f75e1-93a4-49df-8f56-1f2f7ce1a001";
        let log_dir = home.path().join(".intendant").join("logs").join(wrapper_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        let backend_id = "019ea8b9-0000-7000-8000-00000000aa01";
        let body = serde_json::json!({
            "version": 1,
            "wrappers": [{
                "source": "codex",
                "backend_session_id": backend_id,
                "intendant_session_id": wrapper_id,
                "log_path": log_dir.to_string_lossy(),
                "updated_at_secs": 1234,
            }],
        });
        let path = index_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).unwrap();

        let wrappers = wrappers_for(home.path(), "codex", backend_id);
        assert_eq!(wrappers.len(), 1);
        assert_eq!(wrappers[0].state, WrapperState::Active);
        assert_eq!(wrappers[0].rollout_path, None);
        assert_eq!(wrappers[0].updated_at_secs, 1234);
    }

    /// "Active wins": explicit state beats a fresher timestamp; among
    /// active rows the latest `updated_at_secs` wins (beating the lexical
    /// id tie-break); with no active row the best superseded row is the
    /// fallback.
    #[test]
    fn active_wrapper_prefers_state_active_then_latest_updated() {
        let home = tempfile::tempdir().unwrap();
        let backend_id = "019ea8b9-0000-7000-8000-00000000bb02";
        let logs = home.path().join(".intendant").join("logs");
        let rows: Vec<serde_json::Value> = [
            // Superseded row with the freshest timestamp AND the greatest
            // id: only the explicit state keeps it from winning.
            (
                "cccc0000-0000-4000-8000-000000000003",
                9_999_999_999_u64,
                "superseded",
            ),
            ("bbbb0000-0000-4000-8000-000000000002", 100, "active"),
            // Lexically smallest id, but the latest active timestamp: wins.
            ("aaaa0000-0000-4000-8000-000000000001", 200, "active"),
        ]
        .into_iter()
        .map(|(wrapper_id, updated_at_secs, state)| {
            let log_dir = logs.join(wrapper_id);
            std::fs::create_dir_all(&log_dir).unwrap();
            serde_json::json!({
                "source": "codex",
                "backend_session_id": backend_id,
                "intendant_session_id": wrapper_id,
                "log_path": log_dir.to_string_lossy(),
                "updated_at_secs": updated_at_secs,
                "state": state,
            })
        })
        .collect();
        let path = index_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({ "version": 1, "wrappers": rows }).to_string(),
        )
        .unwrap();

        let wrappers = wrappers_for(home.path(), "codex", backend_id);
        assert_eq!(
            wrappers
                .iter()
                .map(|record| record.intendant_session_id.as_str())
                .collect::<Vec<_>>(),
            [
                "aaaa0000-0000-4000-8000-000000000001",
                "bbbb0000-0000-4000-8000-000000000002",
                "cccc0000-0000-4000-8000-000000000003",
            ]
        );
        assert_eq!(
            active_wrapper_in(&wrappers).map(|record| record.intendant_session_id.as_str()),
            Some("aaaa0000-0000-4000-8000-000000000001")
        );
        assert_eq!(
            active_wrapper_for(home.path(), "codex", backend_id)
                .map(|record| record.intendant_session_id),
            Some("aaaa0000-0000-4000-8000-000000000001".to_string())
        );

        // All-superseded fallback: the best superseded row still resolves.
        let backend_two = "019ea8b9-0000-7000-8000-00000000bb03";
        let rows: Vec<serde_json::Value> = [
            ("dddd0000-0000-4000-8000-000000000004", 0_u64),
            ("eeee0000-0000-4000-8000-000000000005", 50),
        ]
        .into_iter()
        .map(|(wrapper_id, updated_at_secs)| {
            let log_dir = logs.join(wrapper_id);
            std::fs::create_dir_all(&log_dir).unwrap();
            serde_json::json!({
                "source": "codex",
                "backend_session_id": backend_two,
                "intendant_session_id": wrapper_id,
                "log_path": log_dir.to_string_lossy(),
                "updated_at_secs": updated_at_secs,
                "state": "superseded",
            })
        })
        .collect();
        std::fs::write(
            &path,
            serde_json::json!({ "version": 1, "wrappers": rows }).to_string(),
        )
        .unwrap();
        assert_eq!(
            active_wrapper_for(home.path(), "codex", backend_two)
                .map(|record| record.intendant_session_id),
            Some("eeee0000-0000-4000-8000-000000000005".to_string())
        );
    }

    #[test]
    fn rollout_path_roundtrip_and_stale_fallback() {
        let home = tempfile::tempdir().unwrap();
        let wrapper_id = "5f8a33fe-6cbb-49f0-8d2a-52a4de17c001";
        let log_dir = home.path().join(".intendant").join("logs").join(wrapper_id);
        std::fs::create_dir_all(&log_dir).unwrap();
        let backend_id = "019ea8b9-0000-7000-8000-00000000cc01";
        upsert(home.path(), "codex", backend_id, wrapper_id, &log_dir, None).unwrap();

        let rollout = home.path().join("rollouts").join("r.jsonl");
        std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        std::fs::write(&rollout, "{\"type\":\"session_meta\"}\n").unwrap();

        // Unknown backend session: the setter no-ops, the resolver stays
        // empty — callers just keep scanning.
        let unknown_backend = "019ea8b9-ffff-7000-8000-00000000cc99";
        record_rollout_path(home.path(), "codex", unknown_backend, &rollout).unwrap();
        assert_eq!(
            resolved_rollout_path(home.path(), "codex", unknown_backend, |_| true),
            None
        );

        // Roundtrip.
        record_rollout_path(home.path(), "codex", backend_id, &rollout).unwrap();
        assert_eq!(
            resolved_rollout_path(home.path(), "codex", backend_id, |_| true),
            Some(rollout.clone())
        );
        // The caller's identity re-verification is a veto.
        assert_eq!(
            resolved_rollout_path(home.path(), "codex", backend_id, |_| false),
            None
        );
        // The stored path is persisted, not cache-only.
        let raw = std::fs::read_to_string(index_path(home.path())).unwrap();
        let disk: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            disk["wrappers"][0]["rollout_path"],
            serde_json::Value::String(rollout.to_string_lossy().to_string())
        );

        // Stale path (file deleted): the resolver declines even with a
        // permissive verify, so callers fall back to the scan.
        std::fs::remove_file(&rollout).unwrap();
        assert_eq!(
            resolved_rollout_path(home.path(), "codex", backend_id, |_| true),
            None
        );
    }
}
