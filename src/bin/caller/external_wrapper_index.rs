use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

const INDEX_FILE: &str = "external_wrapper_index.json";
/// Version 2 marks indexes whose active selection was repaired by
/// [`migrate_index`] after the era when the session-catalog list scan
/// shared the ACTIVATING [`upsert`] (it now uses the non-activating
/// [`backfill`]). Older binaries round-trip the version field they parsed,
/// so a migrated file is not downgraded by their rewrites; fresh indexes
/// are born at the current version and never migrate.
const INDEX_VERSION: u32 = 2;

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
    /// stays written for older readers. (The one sanctioned exception is
    /// the versioned [`migrate_index`] repair, which re-derives every
    /// group's whole selection from log-dir activity rather than patching
    /// individual zeros.)
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

/// The normalized identity for a wrapper-index write, shared by the
/// activating [`upsert`] and the non-activating [`backfill`] so their input
/// guards cannot drift.
struct WrapperWriteIdentity {
    source: String,
    backend_session_id: String,
    stored_intendant_session_id: String,
}

/// Normalize and vet a wrapper-index write. `None` means the write must be
/// skipped: non-external or non-canonical identities, and the attribution
/// guard for callers holding someone else's log dir.
fn validated_write_identity(
    source: &str,
    backend_session_id: &str,
    intendant_session_id: &str,
    log_dir: &Path,
) -> Option<WrapperWriteIdentity> {
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
        return None;
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
        return None;
    }
    Some(WrapperWriteIdentity {
        source,
        backend_session_id: backend_session_id.to_string(),
        stored_intendant_session_id: stored_intendant_session_id.to_string(),
    })
}

pub fn upsert(
    home: &Path,
    source: &str,
    backend_session_id: &str,
    intendant_session_id: &str,
    log_dir: &Path,
    project_root: Option<&Path>,
) -> Result<(), String> {
    let Some(identity) =
        validated_write_identity(source, backend_session_id, intendant_session_id, log_dir)
    else {
        return Ok(());
    };
    let WrapperWriteIdentity {
        source,
        backend_session_id,
        stored_intendant_session_id,
    } = identity;

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

/// Non-activating twin of [`upsert`] for scan-shaped writers (the session
/// catalog's list pass, `web_gateway/session_catalog/external_rows.rs`):
/// it may only insert missing history and refresh a row's own fields — it
/// NEVER flips lifecycle state, never runs the demotion loop, and never
/// resurrects a superseded row's recency.
///
/// The activating [`upsert`] is reserved for the single writer at
/// identity-commit (`session_log/bus_events.rs`): activation asserts "this
/// wrapper is the live one", which only the session that just committed
/// the identity can truthfully say. A list scan re-visits every historical
/// wrapper row and makes no such assertion — while it shared `upsert`,
/// each uncapped pass re-activated rows in directory-scan order and the
/// last-visited group member clobbered the real active wrapper (observed
/// live 2026-07-19: 90 of 165 multi-wrapper `(source, backend)` groups
/// inverted), the demotion loop zeroed every other row's recency, and each
/// visited non-active row forced a full index rewrite.
///
/// Semantics:
/// - exact identity triple present: refresh `log_path` / `project_root`;
///   refresh `updated_at_secs` only when the row is already `Active` — a
///   superseded row's zero sentinel stays (re-stamping it would resurrect
///   the row for legacy readers). State is never touched.
/// - triple absent: insert. `Active` only when the `(source, backend)`
///   group has no row at all AND no active row of this source already
///   carries the wrapper id (the at-most-one-active-per-wrapper-id
///   invariant the reverse lookup in `session_supervisor/launch.rs`
///   depends on); otherwise `Superseded` with the legacy zero sentinel.
/// - nothing changed: no write.
pub fn backfill(
    home: &Path,
    source: &str,
    backend_session_id: &str,
    intendant_session_id: &str,
    log_dir: &Path,
    project_root: Option<&Path>,
) -> Result<(), String> {
    let Some(identity) =
        validated_write_identity(source, backend_session_id, intendant_session_id, log_dir)
    else {
        return Ok(());
    };
    let WrapperWriteIdentity {
        source,
        backend_session_id,
        stored_intendant_session_id,
    } = identity;

    let _guard = INDEX_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let log_path = log_dir.to_string_lossy().to_string();
    let updated_at_secs =
        file_mtime_secs(&log_dir.join("session.jsonl")).max(file_mtime_secs(log_dir));
    let project_root = project_root.map(|path| path.to_string_lossy().to_string());

    let is_exact_identity = |record: &ExternalWrapperRecord| {
        record.source == source
            && record.backend_session_id == backend_session_id
            && record.intendant_session_id == stored_intendant_session_id
    };

    // Decide against the cached index in place and clone only when a write
    // will follow (mirrors `upsert`). A row that already carries exactly
    // this data — including a superseded row whose stale `log_path` fields
    // match — costs no rewrite, which is what keeps an uncapped list pass
    // from rewriting the whole index once per visited non-active row.
    let dirty = with_index_unlocked(home, |index| {
        match index
            .wrappers
            .iter()
            .find(|record| is_exact_identity(record))
        {
            Some(existing) => {
                existing.log_path != log_path
                    || existing.project_root != project_root
                    || (existing.state == WrapperState::Active
                        && existing.updated_at_secs != updated_at_secs)
            }
            None => true,
        }
    });
    if !dirty {
        return Ok(());
    }

    let mut index = with_index_unlocked(home, |index| index.clone());
    let group_has_rows = index
        .wrappers
        .iter()
        .any(|record| record.source == source && record.backend_session_id == backend_session_id);
    let wrapper_active_elsewhere = index.wrappers.iter().any(|record| {
        record.source == source
            && record.intendant_session_id == stored_intendant_session_id
            && record.state == WrapperState::Active
    });
    if let Some(existing) = index
        .wrappers
        .iter_mut()
        .find(|record| is_exact_identity(record))
    {
        existing.log_path = log_path;
        existing.project_root = project_root;
        if existing.state == WrapperState::Active {
            existing.updated_at_secs = updated_at_secs;
        }
    } else if !group_has_rows && !wrapper_active_elsewhere {
        // First record of its backend session, and its wrapper is not the
        // active wrapper of anything else: safe to register as active —
        // there is no selection to change.
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
    } else {
        // Some row already holds this backend session, or this wrapper is
        // already active for another backend: record the history, but only
        // the activating writer may change what is current.
        index.wrappers.push(ExternalWrapperRecord {
            source,
            backend_session_id: backend_session_id.to_string(),
            intendant_session_id: stored_intendant_session_id.to_string(),
            log_path,
            project_root,
            updated_at_secs: 0,
            state: WrapperState::Superseded,
            rollout_path: None,
        });
    }

    write_index_unlocked(home, &index)?;
    note_index_written(home, &index);
    Ok(())
}

/// One-time repair (index version 1 → 2) for indexes damaged while the
/// session-catalog list scan shared the ACTIVATING [`upsert`] (it now uses
/// [`backfill`]): each list pass re-activated historical rows in
/// directory-scan order, leaving most multi-wrapper groups with "active"
/// pointing at a stale wrapper and the real wrapper's recency zeroed.
///
/// The index is derived data, so the repair recomputes activity from the
/// log dirs themselves: per `(source, backend_session_id)` group the row
/// with the freshest log-dir activity mtime becomes the single `Active`
/// row and has its `updated_at_secs` restamped from that mtime; every
/// other group member becomes `Superseded` with the legacy zero sentinel.
/// A group with no surviving log dir keeps no active row (readers already
/// drop rows whose dir is gone). A second pass enforces AT MOST ONE ACTIVE
/// ROW PER `(source, wrapper id)` — the state-independent reverse lookup
/// (wrapper id → backend session, `session_supervisor/launch.rs`) must not
/// resolve a stale backend binding — keeping the freshest row, with the
/// lexically greatest backend session id as the tie-break (time-ordered
/// for Codex UUIDv7 ids, deterministic for the rest).
///
/// Runs only while the on-disk index version is below [`INDEX_VERSION`]
/// and stamps the new version on completion, so it is idempotent: later
/// startups (and fresh indexes, which are born at the current version)
/// skip without touching the file. Returns `None` when skipped, otherwise
/// the number of groups whose active selection changed.
pub fn migrate_index(home: &Path) -> Result<Option<usize>, String> {
    let _guard = INDEX_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let needs_migration = with_index_unlocked(home, |index| index.version < INDEX_VERSION);
    if !needs_migration {
        return Ok(None);
    }
    let mut index = with_index_unlocked(home, |index| index.clone());
    let repaired = repair_active_selection(&mut index.wrappers);
    index.version = INDEX_VERSION;
    write_index_unlocked(home, &index)?;
    note_index_written(home, &index);
    Ok(Some(repaired))
}

/// Startup edge for [`migrate_index`]: resolves the daemon's own home and
/// logs what the one-time repair did (mirrors
/// `global_store::prune_at_daemon_startup`).
pub fn migrate_at_daemon_startup() {
    match migrate_index(&crate::platform::home_dir()) {
        Ok(None) => {}
        Ok(Some(repaired)) => eprintln!(
            "[wrapper-index] migrated external wrapper index to v{INDEX_VERSION} \
             (active selection repaired in {repaired} group(s))"
        ),
        Err(err) => eprintln!("[wrapper-index] external wrapper index migration failed: {err}"),
    }
}

/// The [`migrate_index`] repair pass over the raw rows. Returns the number
/// of `(source, backend_session_id)` groups whose set of active wrapper
/// ids changed.
fn repair_active_selection(wrappers: &mut [ExternalWrapperRecord]) -> usize {
    let activity: Vec<u64> = wrappers
        .iter()
        .map(|record| {
            let dir = Path::new(&record.log_path);
            file_mtime_secs(&dir.join("session.jsonl")).max(file_mtime_secs(dir))
        })
        .collect();
    let before = active_wrapper_ids_by_group(wrappers);

    // Pass 1: per (source, backend session) group, the freshest surviving
    // log dir wins; a group whose dirs are all gone keeps no active row.
    let mut groups: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, record) in wrappers.iter().enumerate() {
        groups
            .entry((record.source.clone(), record.backend_session_id.clone()))
            .or_default()
            .push(i);
    }
    for indices in groups.values() {
        let winner = indices
            .iter()
            .copied()
            .filter(|&i| activity[i] > 0)
            .max_by(|&a, &b| {
                activity[a].cmp(&activity[b]).then_with(|| {
                    wrappers[a]
                        .intendant_session_id
                        .cmp(&wrappers[b].intendant_session_id)
                })
            });
        for &i in indices {
            if Some(i) == winner {
                wrappers[i].state = WrapperState::Active;
                wrappers[i].updated_at_secs = activity[i];
            } else {
                wrappers[i].state = WrapperState::Superseded;
                wrappers[i].updated_at_secs = 0;
            }
        }
    }

    // Pass 2: at most one active row per (source, wrapper id).
    let mut best_by_wrapper: HashMap<(&str, &str), usize> = HashMap::new();
    for (i, record) in wrappers.iter().enumerate() {
        if record.state != WrapperState::Active {
            continue;
        }
        let key = (record.source.as_str(), record.intendant_session_id.as_str());
        let replace = match best_by_wrapper.get(&key) {
            Some(&current) => {
                (activity[i], record.backend_session_id.as_str())
                    > (
                        activity[current],
                        wrappers[current].backend_session_id.as_str(),
                    )
            }
            None => true,
        };
        if replace {
            best_by_wrapper.insert(key, i);
        }
    }
    let keep: std::collections::HashSet<usize> = best_by_wrapper.into_values().collect();
    for (i, record) in wrappers.iter_mut().enumerate() {
        if record.state == WrapperState::Active && !keep.contains(&i) {
            record.state = WrapperState::Superseded;
            record.updated_at_secs = 0;
        }
    }

    let after = active_wrapper_ids_by_group(wrappers);
    before
        .iter()
        .filter(|(group, active_ids)| after.get(*group) != Some(*active_ids))
        .count()
}

/// The sorted active wrapper ids of every `(source, backend_session_id)`
/// group — the "active selection" [`repair_active_selection`] reports
/// changes against.
fn active_wrapper_ids_by_group(
    wrappers: &[ExternalWrapperRecord],
) -> HashMap<(String, String), Vec<String>> {
    let mut map: HashMap<(String, String), Vec<String>> = HashMap::new();
    for record in wrappers {
        let ids = map
            .entry((record.source.clone(), record.backend_session_id.clone()))
            .or_default();
        if record.state == WrapperState::Active {
            ids.push(record.intendant_session_id.clone());
        }
    }
    for ids in map.values_mut() {
        ids.sort();
    }
    map
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

/// Resolve an arbitrary wrapper session id — active OR superseded — to its
/// backend conversation `(source, backend_session_id)`. Deliberately does
/// NOT require the wrapper's log dir to still exist (unlike
/// [`wrappers_for`]): a conversation outlives any one wrapper incarnation,
/// and provenance recorded at park time (agenda items, diary lines) must
/// keep resolving to the conversation after the parking wrapper's dir is
/// pruned. Callers wanting the conversation's *live* row state re-resolve
/// through [`wrappers_for`] with the returned pair.
pub fn conversation_for_wrapper(home: &Path, wrapper_session_id: &str) -> Option<(String, String)> {
    let wrapper_session_id = wrapper_session_id.trim();
    if wrapper_session_id.is_empty() {
        return None;
    }
    let _guard = INDEX_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    with_index_unlocked(home, |index| {
        index
            .wrappers
            .iter()
            .find(|record| record.intendant_session_id == wrapper_session_id)
            .map(|record| (record.source.clone(), record.backend_session_id.clone()))
    })
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

    /// A full session-catalog list pass — [`backfill`] over every
    /// historical row of a multi-wrapper group, in arbitrary order —
    /// changes no active selection and writes nothing. Under the old
    /// shared `upsert` the last-visited group member won "active",
    /// clobbering the resume's correct flip within one pass.
    #[test]
    fn backfill_full_list_pass_preserves_active_selection() {
        let home = tempfile::tempdir().unwrap();
        let old_wrapper = "1a6f0000-0000-4000-8000-000000000a01";
        let new_wrapper = "2b7f0000-0000-4000-8000-000000000a02";
        let logs = home.path().join(".intendant").join("logs");
        let old_dir = logs.join(old_wrapper);
        let new_dir = logs.join(new_wrapper);
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        let backend_id = "019ea8b9-0000-7000-8000-00000000ee01";

        upsert(
            home.path(),
            "codex",
            backend_id,
            old_wrapper,
            &old_dir,
            None,
        )
        .unwrap();
        upsert(
            home.path(),
            "codex",
            backend_id,
            new_wrapper,
            &new_dir,
            None,
        )
        .unwrap();
        let bytes_before = std::fs::read(index_path(home.path())).unwrap();

        // Worst-case order first (stale row visited last), then the other.
        for pass in [[&new_dir, &old_dir], [&old_dir, &new_dir]] {
            for dir in pass {
                let wrapper_id = dir.file_name().unwrap().to_str().unwrap();
                backfill(home.path(), "codex", backend_id, wrapper_id, dir, None).unwrap();
            }
        }

        let wrappers = wrappers_for(home.path(), "codex", backend_id);
        assert_eq!(
            active_wrapper_in(&wrappers).map(|record| record.intendant_session_id.as_str()),
            Some(new_wrapper)
        );
        let old_row = wrappers
            .iter()
            .find(|record| record.intendant_session_id == old_wrapper)
            .unwrap();
        assert_eq!(old_row.state, WrapperState::Superseded);
        assert_eq!(old_row.updated_at_secs, 0);
        // Nothing changed, so the passes wrote nothing at all.
        assert_eq!(
            std::fs::read(index_path(home.path())).unwrap(),
            bytes_before
        );

        // A real field refresh on a superseded row updates the row's own
        // data without resurrecting it.
        let project_root = home.path().join("proj");
        backfill(
            home.path(),
            "codex",
            backend_id,
            old_wrapper,
            &old_dir,
            Some(project_root.as_path()),
        )
        .unwrap();
        let wrappers = wrappers_for(home.path(), "codex", backend_id);
        assert_eq!(
            active_wrapper_in(&wrappers).map(|record| record.intendant_session_id.as_str()),
            Some(new_wrapper)
        );
        let old_row = wrappers
            .iter()
            .find(|record| record.intendant_session_id == old_wrapper)
            .unwrap();
        assert_eq!(old_row.state, WrapperState::Superseded);
        assert_eq!(old_row.updated_at_secs, 0);
        assert_eq!(
            old_row.project_root.as_deref(),
            Some(project_root.to_string_lossy().as_ref())
        );
    }

    /// [`backfill`] inserts a missing triple as `Active` only when there is
    /// no selection to change: the backend-session group must be empty and
    /// the wrapper must not be active for another backend session.
    #[test]
    fn backfill_insert_is_active_only_when_group_is_empty() {
        let home = tempfile::tempdir().unwrap();
        let wrapper_one = "3c8f0000-0000-4000-8000-000000000b01";
        let wrapper_two = "4d9f0000-0000-4000-8000-000000000b02";
        let logs = home.path().join(".intendant").join("logs");
        let dir_one = logs.join(wrapper_one);
        let dir_two = logs.join(wrapper_two);
        std::fs::create_dir_all(&dir_one).unwrap();
        std::fs::create_dir_all(&dir_two).unwrap();
        let backend_x = "019ea8b9-0000-7000-8000-00000000ee11";
        let backend_y = "019ea8b9-0000-7000-8000-00000000ee12";

        // Empty group: the insert may claim active.
        backfill(home.path(), "codex", backend_x, wrapper_one, &dir_one, None).unwrap();
        let wrappers = wrappers_for(home.path(), "codex", backend_x);
        assert_eq!(wrappers.len(), 1);
        assert_eq!(wrappers[0].state, WrapperState::Active);
        assert!(wrappers[0].updated_at_secs > 0);

        // Populated group: history only — sentinel and superseded.
        backfill(home.path(), "codex", backend_x, wrapper_two, &dir_two, None).unwrap();
        let wrappers = wrappers_for(home.path(), "codex", backend_x);
        assert_eq!(wrappers.len(), 2);
        assert_eq!(
            active_wrapper_in(&wrappers).map(|record| record.intendant_session_id.as_str()),
            Some(wrapper_one)
        );
        let second = wrappers
            .iter()
            .find(|record| record.intendant_session_id == wrapper_two)
            .unwrap();
        assert_eq!(second.state, WrapperState::Superseded);
        assert_eq!(second.updated_at_secs, 0);

        // Empty group, but the wrapper is already active for backend_x: the
        // insert must not mint a second active row for the wrapper id — the
        // state-independent reverse lookup (wrapper id -> backend session)
        // must keep resolving the original binding.
        backfill(home.path(), "codex", backend_y, wrapper_one, &dir_one, None).unwrap();
        let inserted = wrappers_for(home.path(), "codex", backend_y);
        assert_eq!(inserted.len(), 1);
        assert_eq!(inserted[0].state, WrapperState::Superseded);
        assert_eq!(inserted[0].updated_at_secs, 0);
        let by_wrapper = wrappers_for_source(home.path(), "codex")
            .into_iter()
            .find(|record| record.intendant_session_id == wrapper_one)
            .unwrap();
        assert_eq!(by_wrapper.backend_session_id, backend_x);
    }

    /// The not-dirty fast path: re-backfilling rows that already carry the
    /// visited data — active or superseded — leaves the index file
    /// untouched (no rewrite churn from list passes).
    #[test]
    fn backfill_unchanged_rows_do_not_rewrite_the_index_file() {
        let home = tempfile::tempdir().unwrap();
        let old_wrapper = "5e1f0000-0000-4000-8000-000000000c01";
        let new_wrapper = "6f2f0000-0000-4000-8000-000000000c02";
        let logs = home.path().join(".intendant").join("logs");
        let old_dir = logs.join(old_wrapper);
        let new_dir = logs.join(new_wrapper);
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        let backend_id = "019ea8b9-0000-7000-8000-00000000ee21";
        upsert(
            home.path(),
            "codex",
            backend_id,
            old_wrapper,
            &old_dir,
            None,
        )
        .unwrap();
        upsert(
            home.path(),
            "codex",
            backend_id,
            new_wrapper,
            &new_dir,
            None,
        )
        .unwrap();

        let path = index_path(home.path());
        let bytes = std::fs::read(&path).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        // Active row with an unchanged activity mtime, then a superseded
        // row whose sentinel deliberately differs from the dir's live
        // mtime: neither is dirty.
        backfill(
            home.path(),
            "codex",
            backend_id,
            new_wrapper,
            &new_dir,
            None,
        )
        .unwrap();
        backfill(
            home.path(),
            "codex",
            backend_id,
            old_wrapper,
            &old_dir,
            None,
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
    }

    fn set_file_mtime(path: &Path, mtime: std::time::SystemTime) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    fn secs_since_epoch(time: std::time::SystemTime) -> u64 {
        time.duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    /// The v1 -> v2 migration re-derives each group's active row from
    /// log-dir activity: the freshest surviving wrapper wins and gets its
    /// recency restamped, stale "actives" are demoted to the sentinel, and
    /// groups with no surviving log dir keep no active row. Already-current
    /// indexes are skipped byte-for-byte.
    #[test]
    fn migration_repairs_inverted_groups_and_restamps_recency() {
        let home = tempfile::tempdir().unwrap();
        let logs = home.path().join(".intendant").join("logs");
        let stale_wrapper = "7a3f0000-0000-4000-8000-000000000d01";
        let fresh_wrapper = "8b4f0000-0000-4000-8000-000000000d02";
        let backend_id = "019ea8b9-0000-7000-8000-00000000ee31";
        let dead_wrapper = "9c5f0000-0000-4000-8000-000000000d03";
        let dead_backend = "019ea8b9-0000-7000-8000-00000000ee32";

        // Future mtimes make the ordering independent of the dirs' own
        // creation times (activity = max(session.jsonl, dir)).
        let base = std::time::SystemTime::now() + std::time::Duration::from_secs(1_000_000);
        let fresh_time = base + std::time::Duration::from_secs(500);
        for (wrapper, mtime) in [(stale_wrapper, base), (fresh_wrapper, fresh_time)] {
            let dir = logs.join(wrapper);
            std::fs::create_dir_all(&dir).unwrap();
            let log = dir.join("session.jsonl");
            std::fs::write(&log, "{}\n").unwrap();
            set_file_mtime(&log, mtime);
        }

        let row = |wrapper: &str, backend: &str, updated: u64, state: &str| {
            serde_json::json!({
                "source": "codex",
                "backend_session_id": backend,
                "intendant_session_id": wrapper,
                "log_path": logs.join(wrapper).to_string_lossy(),
                "updated_at_secs": updated,
                "state": state,
            })
        };
        let path = index_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "version": 1,
                "wrappers": [
                    // Inverted group: the stale wrapper holds "active", the
                    // fresh one was demoted to the sentinel by a list pass.
                    row(stale_wrapper, backend_id, 1_000, "active"),
                    row(fresh_wrapper, backend_id, 0, "superseded"),
                    // Dead group: its "active" row's log dir is gone.
                    row(dead_wrapper, dead_backend, 2_000, "active"),
                ],
            })
            .to_string(),
        )
        .unwrap();

        assert_eq!(migrate_index(home.path()).unwrap(), Some(2));

        let wrappers = wrappers_for(home.path(), "codex", backend_id);
        assert_eq!(wrappers.len(), 2);
        assert_eq!(wrappers[0].intendant_session_id, fresh_wrapper);
        assert_eq!(wrappers[0].state, WrapperState::Active);
        assert_eq!(wrappers[0].updated_at_secs, secs_since_epoch(fresh_time));
        assert_eq!(wrappers[1].intendant_session_id, stale_wrapper);
        assert_eq!(wrappers[1].state, WrapperState::Superseded);
        assert_eq!(wrappers[1].updated_at_secs, 0);

        // On disk: version stamped, and the dead group's row was demoted
        // (its dir being gone hides it from `wrappers_for`).
        let disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(disk["version"], 2);
        let dead_row = disk["wrappers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["intendant_session_id"] == dead_wrapper)
            .unwrap();
        assert_eq!(dead_row["state"], "superseded");
        assert_eq!(dead_row["updated_at_secs"], 0);

        // Idempotent: a second run skips without touching the file.
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(migrate_index(home.path()).unwrap(), None);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    /// The migration's second pass: a wrapper id may keep at most one
    /// active row across backend-session groups, so the state-independent
    /// reverse lookup (wrapper id -> backend session) cannot resolve a
    /// stale backend binding. On equal activity the lexically greatest
    /// backend session id wins (time-ordered for Codex UUIDv7 ids).
    #[test]
    fn migration_enforces_at_most_one_active_row_per_wrapper_id() {
        let home = tempfile::tempdir().unwrap();
        let logs = home.path().join(".intendant").join("logs");
        let wrapper_id = "1d6f0000-0000-4000-8000-000000000e01";
        let old_backend = "019ea8b9-0000-7000-8000-00000000ee41";
        let new_backend = "019ea8b9-0000-7000-8000-00000000ee42";
        let dir = logs.join(wrapper_id);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("session.jsonl");
        std::fs::write(&log, "{}\n").unwrap();
        set_file_mtime(
            &log,
            std::time::SystemTime::now() + std::time::Duration::from_secs(1_000_000),
        );

        let row = |backend: &str| {
            serde_json::json!({
                "source": "codex",
                "backend_session_id": backend,
                "intendant_session_id": wrapper_id,
                "log_path": dir.to_string_lossy(),
                "updated_at_secs": 100,
                "state": "active",
            })
        };
        let path = index_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({ "version": 1, "wrappers": [row(old_backend), row(new_backend)] })
                .to_string(),
        )
        .unwrap();

        // One group's selection changes (the old backend loses its active).
        assert_eq!(migrate_index(home.path()).unwrap(), Some(1));

        let records = wrappers_for_source(home.path(), "codex");
        let active: Vec<_> = records
            .iter()
            .filter(|record| record.state == WrapperState::Active)
            .collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].backend_session_id, new_backend);
        // The preference-ordered reverse lookup resolves the kept binding.
        assert_eq!(
            records
                .iter()
                .find(|record| record.intendant_session_id == wrapper_id)
                .map(|record| record.backend_session_id.as_str()),
            Some(new_backend)
        );
    }

    /// An index already at the current version is never rewritten — the
    /// migration must not re-litigate selections the activating writer has
    /// made since.
    #[test]
    fn migration_skips_index_already_at_current_version() {
        let home = tempfile::tempdir().unwrap();
        let logs = home.path().join(".intendant").join("logs");
        let wrapper_id = "2e7f0000-0000-4000-8000-000000000f01";
        let dir = logs.join(wrapper_id);
        std::fs::create_dir_all(&dir).unwrap();
        let path = index_path(home.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "version": 2,
                "wrappers": [{
                    "source": "codex",
                    "backend_session_id": "019ea8b9-0000-7000-8000-00000000ee51",
                    "intendant_session_id": wrapper_id,
                    "log_path": dir.to_string_lossy(),
                    // Deliberately odd (sentinel recency on an active row):
                    // even a repair-shaped file is left alone once stamped.
                    "updated_at_secs": 0,
                    "state": "active",
                }],
            })
            .to_string(),
        )
        .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(migrate_index(home.path()).unwrap(), None);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        // A missing index is equally a no-op — nothing is created.
        let empty_home = tempfile::tempdir().unwrap();
        assert_eq!(migrate_index(empty_home.path()).unwrap(), None);
        assert!(!index_path(empty_home.path()).exists());
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
