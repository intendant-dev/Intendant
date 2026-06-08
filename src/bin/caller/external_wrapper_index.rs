use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

const INDEX_FILE: &str = "external_wrapper_index.json";
const INDEX_VERSION: u32 = 1;

static INDEX_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExternalWrapperRecord {
    pub source: String,
    pub backend_session_id: String,
    pub intendant_session_id: String,
    pub log_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_root: Option<String>,
    #[serde(default)]
    pub updated_at_secs: u64,
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
    home.join(".intendant").join(INDEX_FILE)
}

pub fn home_from_log_dir(log_dir: &Path) -> Option<PathBuf> {
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

    let _guard = INDEX_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut index = read_index_unlocked(home);
    let log_path = log_dir.to_string_lossy().to_string();
    let updated_at_secs =
        file_mtime_secs(&log_dir.join("session.jsonl")).max(file_mtime_secs(log_dir));
    let project_root = project_root.map(|path| path.to_string_lossy().to_string());

    for record in index.wrappers.iter_mut().filter(|record| {
        record.source == source
            && record.backend_session_id == backend_session_id
            && record.intendant_session_id != stored_intendant_session_id
    }) {
        record.updated_at_secs = 0;
    }

    if let Some(existing) = index.wrappers.iter_mut().find(|record| {
        record.source == source
            && record.backend_session_id == backend_session_id
            && record.intendant_session_id == stored_intendant_session_id
    }) {
        existing.log_path = log_path;
        existing.project_root = project_root;
        existing.updated_at_secs = updated_at_secs;
    } else {
        index.wrappers.push(ExternalWrapperRecord {
            source,
            backend_session_id: backend_session_id.to_string(),
            intendant_session_id: stored_intendant_session_id.to_string(),
            log_path,
            project_root,
            updated_at_secs,
        });
    }

    write_index_unlocked(home, &index)
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
    let mut records: Vec<_> = read_index_unlocked(home)
        .wrappers
        .into_iter()
        .filter_map(|record| {
            (record.source == source
                && record.backend_session_id == backend_session_id
                && Path::new(&record.log_path).is_dir())
            .then(|| normalize_log_identity(record))
            .flatten()
        })
        .collect();
    records.sort_by(|a, b| {
        b.updated_at_secs
            .cmp(&a.updated_at_secs)
            .then_with(|| b.intendant_session_id.cmp(&a.intendant_session_id))
    });
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
    let mut records: Vec<_> = read_index_unlocked(home)
        .wrappers
        .into_iter()
        .filter_map(|record| {
            (record.source == source && Path::new(&record.log_path).is_dir())
                .then(|| normalize_log_identity(record))
                .flatten()
        })
        .collect();
    records.sort_by(|a, b| {
        b.updated_at_secs
            .cmp(&a.updated_at_secs)
            .then_with(|| b.intendant_session_id.cmp(&a.intendant_session_id))
    });
    records
}

pub fn record_to_json(record: &ExternalWrapperRecord) -> serde_json::Value {
    serde_json::json!({
        "source": record.source,
        "backend_session_id": record.backend_session_id,
        "intendant_session_id": record.intendant_session_id,
        "path": record.log_path,
        "project_root": record.project_root,
        "updated_at_secs": record.updated_at_secs,
    })
}

fn read_index_unlocked(home: &Path) -> ExternalWrapperIndex {
    let path = index_path(home);
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
        assert_eq!(
            wrappers[1].intendant_session_id,
            "e9532107-8c7f-4c1f-b88d-410d6d365505"
        );
        assert_eq!(wrappers[1].updated_at_secs, 0);

        let source_wrappers = wrappers_for_source(home.path(), "codex");
        assert_eq!(
            source_wrappers
                .first()
                .map(|record| record.intendant_session_id.as_str()),
            Some("ec5865e5-a5af-4b8c-81a1-545a3a6f8ba9")
        );
    }
}
