//! Daemon-side per-backend model catalogs learned from live backend runs
//! (card 01KZR0QP9A — the stale-model-catalog session-death class).
//!
//! The dashboard used to hardcode each backend's model vocabulary in the
//! SPA. When a backend's real vocabulary moved (Kimi 0.34 refuses model ids
//! its older builds shipped with, and a fresh install starts with an empty
//! configured-model list), a picker built from that mirror produced launches
//! that spawned and then died on the backend's "model not configured"
//! refusal. This module is the daemon's single source of model-vocabulary
//! truth for backends whose catalog can only be learned from their own
//! running harness:
//!
//! - **Capture**: an adapter that reaches its backend's catalog service
//!   records what the backend actually reported (the Kimi adapter records
//!   `modelResolver.listModels` at every successful spawn). Nothing is ever
//!   fabricated — an empty list is a legitimate catalog (a fresh Kimi
//!   install has no configured models).
//! - **Serve**: the `/api/external-agents` availability rows carry the
//!   stored catalog (`models` + provenance) when one is known, and an
//!   honest `models: null` + `models_reason` when it is not; every SPA
//!   model picker derives from that payload ("derive, don't mirror").
//! - **Validate**: the session-launch path refuses a model pin that a
//!   KNOWN catalog does not contain — before any session spawns — naming
//!   what is available. An unknown catalog never refuses (and never
//!   invents); the adapter's mid-launch degrade covers that path.
//!
//! Storage is `<state root>/external-agents/model-catalogs.json` plus a
//! process-wide cache keyed by state root (a real daemon has exactly one;
//! tests inject tempdir roots — the hermeticity convention).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Backends whose model catalog this daemon can learn (and therefore serve
/// and validate). Only these ids get a `models`/`models_reason` field on
/// their availability row — advertising an always-null catalog for a
/// backend the daemon never captures would misread as "a run will fill
/// this in".
pub(crate) const CATALOG_CAPABLE_BACKEND_IDS: &[&str] = &["kimi"];

/// One model choice as the backend itself reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CatalogModel {
    /// The exact id the backend accepts as a model selection
    /// (e.g. Kimi's `kimi-code/k3`).
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_context_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) support_efforts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_effort: Option<String>,
}

/// One backend's stored catalog with capture provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BackendModelCatalog {
    pub(crate) models: Vec<CatalogModel>,
    /// The backend server version that reported this catalog, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) server_version: Option<String>,
    pub(crate) captured_at_epoch_secs: u64,
}

/// Disk format under `<state root>/external-agents/model-catalogs.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CatalogFile {
    version: u32,
    #[serde(default)]
    backends: HashMap<String, BackendModelCatalog>,
}

const CATALOG_FILE_VERSION: u32 = 1;
const MAX_CATALOG_FILE_BYTES: u64 = 1024 * 1024;

fn catalog_path(state_root: &Path) -> PathBuf {
    state_root
        .join("external-agents")
        .join("model-catalogs.json")
}

/// Process-wide cache of loaded catalog files, keyed by state root so tests
/// with injected tempdir roots never observe each other or the live daemon.
fn store() -> &'static Mutex<HashMap<PathBuf, HashMap<String, BackendModelCatalog>>> {
    static STORE: OnceLock<Mutex<HashMap<PathBuf, HashMap<String, BackendModelCatalog>>>> =
        OnceLock::new();
    STORE.get_or_init(Default::default)
}

fn load_from_disk(state_root: &Path) -> HashMap<String, BackendModelCatalog> {
    let path = catalog_path(state_root);
    let oversized = std::fs::metadata(&path)
        .map(|meta| meta.len() > MAX_CATALOG_FILE_BYTES)
        .unwrap_or(false);
    if oversized {
        return HashMap::new();
    }
    // A missing or malformed file is honestly "no catalog known" — never a
    // fabricated list, and never a hard error on a serving path.
    std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<CatalogFile>(&bytes).ok())
        .filter(|file| file.version == CATALOG_FILE_VERSION)
        .map(|file| file.backends)
        .unwrap_or_default()
}

fn with_loaded<R>(
    state_root: &Path,
    body: impl FnOnce(&mut HashMap<String, BackendModelCatalog>) -> R,
) -> R {
    let mut store = store().lock().expect("model catalog store poisoned");
    let entry = store
        .entry(state_root.to_path_buf())
        .or_insert_with(|| load_from_disk(state_root));
    body(entry)
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Record the catalog a live backend run reported. Returns `true` when the
/// stored catalog changed (new backend, different models, or a different
/// server version) — the signal serving layers use to re-broadcast.
/// Failures to persist are non-fatal: the in-memory catalog still serves
/// this daemon run, and the next capture retries the write.
pub(crate) fn record_backend_models(
    state_root: &Path,
    backend_id: &str,
    models: Vec<CatalogModel>,
    server_version: Option<String>,
) -> bool {
    record_backend_models_at(state_root, backend_id, models, server_version, now_epoch_secs())
}

pub(crate) fn record_backend_models_at(
    state_root: &Path,
    backend_id: &str,
    models: Vec<CatalogModel>,
    server_version: Option<String>,
    now_epoch_secs: u64,
) -> bool {
    with_loaded(state_root, |backends| {
        let unchanged = backends.get(backend_id).is_some_and(|existing| {
            existing.models == models && existing.server_version == server_version
        });
        if unchanged {
            return false;
        }
        backends.insert(
            backend_id.to_string(),
            BackendModelCatalog {
                models,
                server_version,
                captured_at_epoch_secs: now_epoch_secs,
            },
        );
        let file = CatalogFile {
            version: CATALOG_FILE_VERSION,
            backends: backends.clone(),
        };
        let path = catalog_path(state_root);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_vec_pretty(&file) {
            Ok(bytes) => {
                if let Err(error) = crate::file_watcher::atomic_write(&path, &bytes) {
                    eprintln!(
                        "[model-catalog] {backend_id} catalog was not persisted to {}: {error}",
                        path.display()
                    );
                }
            }
            Err(error) => {
                eprintln!("[model-catalog] {backend_id} catalog did not encode: {error}");
            }
        }
        true
    })
}

/// The stored catalog for one backend, if any run has ever reported one.
pub(crate) fn backend_catalog(state_root: &Path, backend_id: &str) -> Option<BackendModelCatalog> {
    with_loaded(state_root, |backends| backends.get(backend_id).cloned())
}

/// The availability-row projection: `(models value, reason-when-null)`.
///
/// `models` is `null` when no catalog was ever captured; the caller (which
/// knows `installed`) picks between the two honest reasons. When a catalog
/// exists — including a legitimately EMPTY one (fresh Kimi installs have no
/// configured models) — it is served with capture provenance.
pub(crate) fn row_models_json(
    state_root: &Path,
    backend_id: &str,
    installed: bool,
) -> (serde_json::Value, Option<&'static str>) {
    row_models_json_at(state_root, backend_id, installed, now_epoch_secs())
}

pub(crate) fn row_models_json_at(
    state_root: &Path,
    backend_id: &str,
    installed: bool,
    now_epoch_secs: u64,
) -> (serde_json::Value, Option<&'static str>) {
    match backend_catalog(state_root, backend_id) {
        Some(catalog) => {
            let value = serde_json::json!({
                "list": catalog.models,
                "server_version": catalog.server_version,
                "captured_secs_ago":
                    now_epoch_secs.saturating_sub(catalog.captured_at_epoch_secs),
            });
            (value, None)
        }
        None => {
            let reason = if installed {
                "no-run-observed"
            } else {
                "not-installed"
            };
            (serde_json::Value::Null, Some(reason))
        }
    }
}

/// Fingerprint of one backend's stored catalog for change detection on the
/// serving side (`None` = no catalog captured).
pub(crate) fn catalog_fingerprint(state_root: &Path, backend_id: &str) -> Option<u64> {
    backend_catalog(state_root, backend_id).map(|catalog| catalog.captured_at_epoch_secs)
}

/// Launch-gate verdict for a Kimi model pin: `Some(refusal message)` when
/// the daemon KNOWS Kimi's catalog and the pin is not in it. An unknown
/// catalog returns `None` — the daemon must never refuse (or approve) from
/// invented knowledge; the adapter's mid-launch degrade covers that case.
pub(crate) fn kimi_launch_model_refusal(state_root: &Path, model: &str) -> Option<String> {
    launch_model_refusal(backend_catalog(state_root, "kimi").as_ref(), "Kimi", model)
}

fn launch_model_refusal(
    catalog: Option<&BackendModelCatalog>,
    backend_label: &str,
    model: &str,
) -> Option<String> {
    let catalog = catalog?;
    if catalog.models.iter().any(|entry| entry.id == model) {
        return None;
    }
    let provenance = match catalog.server_version.as_deref() {
        Some(version) => format!("{backend_label} {version} reported"),
        None => format!("the last {backend_label} run reported"),
    };
    if catalog.models.is_empty() {
        return Some(format!(
            "{backend_label} model \"{model}\" is not configured: {provenance} an empty \
             configured-model catalog (a fresh install has none). Launch without a model pin \
             to use the backend default, or configure models in {backend_label}'s config; \
             running any {backend_label} session refreshes this catalog."
        ));
    }
    let available = catalog
        .models
        .iter()
        .map(|entry| entry.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "{backend_label} model \"{model}\" is not configured in this {backend_label} install. \
         {provenance} these models: {available}. Pick one of those, launch without a model pin \
         for the backend default, or update {backend_label}'s config (running any \
         {backend_label} session refreshes this catalog)."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> CatalogModel {
        CatalogModel {
            id: id.to_string(),
            display_name: None,
            max_context_size: None,
            support_efforts: None,
            default_effort: None,
        }
    }

    fn catalog(models: Vec<CatalogModel>, server_version: Option<&str>) -> BackendModelCatalog {
        BackendModelCatalog {
            models,
            server_version: server_version.map(str::to_string),
            captured_at_epoch_secs: 1_700_000_000,
        }
    }

    #[test]
    fn record_and_read_roundtrip_survives_a_fresh_store() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(backend_catalog(root.path(), "kimi"), None);
        let changed = record_backend_models_at(
            root.path(),
            "kimi",
            vec![model("kimi-code/k3"), model("kimi-code/k3-256k")],
            Some("0.34.0".to_string()),
            1_700_000_123,
        );
        assert!(changed);
        let stored = backend_catalog(root.path(), "kimi").unwrap();
        assert_eq!(stored.models.len(), 2);
        assert_eq!(stored.server_version.as_deref(), Some("0.34.0"));
        assert_eq!(stored.captured_at_epoch_secs, 1_700_000_123);

        // A second identical capture is not a change (no re-broadcast).
        let unchanged = record_backend_models_at(
            root.path(),
            "kimi",
            vec![model("kimi-code/k3"), model("kimi-code/k3-256k")],
            Some("0.34.0".to_string()),
            1_700_000_999,
        );
        assert!(!unchanged);
        // The persisted file is valid JSON in the declared format.
        let bytes = std::fs::read(catalog_path(root.path())).unwrap();
        let file: CatalogFile = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(file.version, CATALOG_FILE_VERSION);
        assert!(file.backends.contains_key("kimi"));
    }

    #[test]
    fn disk_catalog_survives_a_fresh_memory_store() {
        // Simulate a daemon restart: write through one state root, then read
        // through a DIFFERENT (but same-path) entry by evicting the cache.
        let root = tempfile::tempdir().unwrap();
        record_backend_models_at(
            root.path(),
            "kimi",
            vec![model("kimi-code/kimi-for-coding")],
            None,
            42,
        );
        store()
            .lock()
            .unwrap()
            .remove(&root.path().to_path_buf());
        let stored = backend_catalog(root.path(), "kimi").unwrap();
        assert_eq!(stored.models[0].id, "kimi-code/kimi-for-coding");
        assert_eq!(stored.captured_at_epoch_secs, 42);
    }

    #[test]
    fn malformed_or_oversized_files_read_as_no_catalog() {
        let root = tempfile::tempdir().unwrap();
        let path = catalog_path(root.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(backend_catalog(root.path(), "kimi"), None);

        // Unknown future version: served as unknown, not misread.
        let root2 = tempfile::tempdir().unwrap();
        let path2 = catalog_path(root2.path());
        std::fs::create_dir_all(path2.parent().unwrap()).unwrap();
        std::fs::write(
            &path2,
            serde_json::json!({"version": 999, "backends": {"kimi": {"models": [], "captured_at_epoch_secs": 1}}})
                .to_string(),
        )
        .unwrap();
        assert_eq!(backend_catalog(root2.path(), "kimi"), None);
    }

    #[test]
    fn row_projection_serves_catalog_or_honest_reason() {
        let root = tempfile::tempdir().unwrap();
        // No catalog: null + reason keyed on installedness.
        let (value, reason) = row_models_json_at(root.path(), "kimi", false, 100);
        assert!(value.is_null());
        assert_eq!(reason, Some("not-installed"));
        let (value, reason) = row_models_json_at(root.path(), "kimi", true, 100);
        assert!(value.is_null());
        assert_eq!(reason, Some("no-run-observed"));

        // Captured catalog (EMPTY is a real catalog — fresh installs).
        record_backend_models_at(root.path(), "kimi", Vec::new(), Some("0.34.0".into()), 50);
        let (value, reason) = row_models_json_at(root.path(), "kimi", true, 80);
        assert_eq!(reason, None);
        assert_eq!(value["list"], serde_json::json!([]));
        assert_eq!(value["server_version"], "0.34.0");
        assert_eq!(value["captured_secs_ago"], 30);
    }

    #[test]
    fn unknown_catalog_never_refuses_a_launch() {
        assert_eq!(launch_model_refusal(None, "Kimi", "kimi-code/k3"), None);
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            kimi_launch_model_refusal(root.path(), "kimi-code/k3"),
            None
        );
    }

    #[test]
    fn known_catalog_accepts_members_and_refuses_others_naming_what_exists() {
        let catalog = catalog(
            vec![model("kimi-code/k3"), model("kimi-code/k3-256k")],
            Some("0.34.0"),
        );
        assert_eq!(
            launch_model_refusal(Some(&catalog), "Kimi", "kimi-code/k3"),
            None
        );
        let refusal =
            launch_model_refusal(Some(&catalog), "Kimi", "kimi-code/kimi-for-coding").unwrap();
        assert!(refusal.contains("kimi-code/kimi-for-coding"), "{refusal}");
        assert!(refusal.contains("kimi-code/k3, kimi-code/k3-256k"), "{refusal}");
        assert!(refusal.contains("0.34.0"), "{refusal}");
        assert!(refusal.contains("without a model pin"), "{refusal}");
    }

    #[test]
    fn empty_known_catalog_refuses_every_pin_with_fresh_install_copy() {
        let catalog = catalog(Vec::new(), Some("0.34.0"));
        let refusal = launch_model_refusal(Some(&catalog), "Kimi", "kimi-code/k3").unwrap();
        assert!(refusal.contains("empty"), "{refusal}");
        assert!(refusal.contains("fresh install"), "{refusal}");
        assert!(refusal.contains("without a model pin"), "{refusal}");
    }

    #[test]
    fn catalog_capable_ids_match_the_backend_vocabulary() {
        for id in CATALOG_CAPABLE_BACKEND_IDS {
            let backend = crate::external_agent::AgentBackend::from_str_loose(id)
                .unwrap_or_else(|| panic!("{id} is not a known backend id"));
            assert_eq!(backend.as_short_str(), *id);
        }
    }
}
