//! Bundled first-party plugin registry: descriptors, persisted enable
//! state, readiness, and the skill payloads a plugin materializes.
//!
//! A plugin here is a declarative bundle — a dashboard catalog card, agent
//! skill payloads, and a bounded readiness probe — not a runtime extension
//! system. The catalog UI is host-owned; plugins supply no code, hooks, or
//! frames. Payload bytes are embedded at compile time from `plugins/` (the
//! parity test pins them to the packages on disk), but unlike
//! [`crate::builtin_skills`], they are materialized into the global skill
//! roots only while their plugin is enabled AND its readiness probe
//! passes, and are swept again when either stops being true — a skill that
//! teaches a dead lane is worse than no skill.
//!
//! Enable state is one small versioned JSON file under the daemon state
//! root. Loads are tolerant (missing/corrupt/foreign version ⇒ default),
//! writes are private and atomic, and ids this binary does not recognize
//! are preserved across rewrites so an older daemon never disarms a newer
//! one's plugins.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::builtin_skills::BuiltinSkill;
use crate::cu_readiness::{LayerStatus, ReadinessLayer};

/// Stable id of the first bundled plugin.
pub(crate) const REMOTE_COMPUTE_PLUGIN_ID: &str = "codex-cloud-remote-compute";

/// One bundled first-party plugin.
pub(crate) struct BundledPlugin {
    /// Stable id; equals the package directory name under `plugins/` and
    /// the `name` field of its `.codex-plugin/plugin.json`.
    pub(crate) id: &'static str,
    pub(crate) display_name: &'static str,
    /// One-paragraph catalog card summary.
    pub(crate) summary: &'static str,
    /// Skill payloads materialized while the plugin is active.
    pub(crate) skills: &'static [BuiltinSkill],
}

/// Every plugin bundled into this binary, in catalog order.
pub(crate) const BUNDLED_PLUGINS: &[BundledPlugin] = &[BundledPlugin {
    id: REMOTE_COMPUTE_PLUGIN_ID,
    display_name: "Codex Cloud Remote Compute",
    summary: "Offload heavy platform-neutral development work (builds, broad \
              tests, lint, benchmarks, codegen) to Codex Cloud workers through \
              the provider-neutral remote-compute lane (`intendant ctl \
              remote`). Enabling activates a shared agent skill for every \
              session kind on this machine.",
    skills: &[BuiltinSkill {
        name: "intendant-remote-compute",
        skill_md: include_str!(
            "../../../plugins/codex-cloud-remote-compute/skills/intendant-remote-compute/SKILL.md"
        ),
        support_files: &[],
    }],
}];

pub(crate) fn bundled_plugin(id: &str) -> Option<&'static BundledPlugin> {
    BUNDLED_PLUGINS.iter().find(|plugin| plugin.id == id)
}

// ── Persisted enable state ──────────────────────────────────────────────────

const STATE_VERSION: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize)]
struct PluginStateFile {
    version: u32,
    #[serde(default)]
    enabled: BTreeSet<String>,
}

/// `<state_root>/plugins/state.json`.
pub(crate) fn plugin_state_path_in(state_root: &Path) -> PathBuf {
    state_root.join("plugins").join("state.json")
}

/// Enabled plugin ids as persisted — including ids this binary does not
/// bundle. Tolerant: missing or unreadable state reads as "nothing
/// enabled" (the shipped default).
pub(crate) fn enabled_plugins_in(state_root: &Path) -> BTreeSet<String> {
    let raw = match std::fs::read(plugin_state_path_in(state_root)) {
        Ok(raw) => raw,
        Err(_) => return BTreeSet::new(),
    };
    match serde_json::from_slice::<PluginStateFile>(&raw) {
        Ok(state) if state.version == STATE_VERSION => state.enabled,
        _ => BTreeSet::new(),
    }
}

/// Persist one plugin's enabled flag. Unknown ids are rejected; ids from
/// other (newer) binaries already in the file are carried through
/// untouched. The write is private (0600) and atomic (tmp + rename).
pub(crate) fn set_plugin_enabled_in(
    state_root: &Path,
    plugin_id: &str,
    enabled: bool,
) -> Result<(), String> {
    if bundled_plugin(plugin_id).is_none() {
        return Err(format!("unknown plugin id '{plugin_id}'"));
    }
    let mut set = enabled_plugins_in(state_root);
    let changed = if enabled {
        set.insert(plugin_id.to_string())
    } else {
        set.remove(plugin_id)
    };
    if !changed {
        return Ok(());
    }
    let path = plugin_state_path_in(state_root);
    let parent = path
        .parent()
        .ok_or_else(|| "plugin state path has no parent".to_string())?;
    intendant_core::state_paths::create_private_dir_all(parent)
        .map_err(|error| format!("create {}: {error}", parent.display()))?;
    let state = PluginStateFile {
        version: STATE_VERSION,
        enabled: set,
    };
    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|error| format!("encode plugin state: {error}"))?;
    let tmp = path.with_extension("json.tmp");
    intendant_core::state_paths::write_private_file(&tmp, &bytes)
        .map_err(|error| format!("write {}: {error}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|error| format!("rename {}: {error}", path.display()))
}

// ── Readiness ───────────────────────────────────────────────────────────────

/// Stable layer ids for the remote-compute readiness report.
pub(crate) const LAYER_ENVIRONMENT: &str = "codex_cloud_environment";
pub(crate) const LAYER_HOME_URL: &str = "daemon_home_url";
pub(crate) const LAYER_TLS_IDENTITY: &str = "gateway_tls_identity";

/// Bounded plugin readiness: cheap, read-only, never cached. Probes only
/// local state (env, config files, the in-memory attach registry) — a
/// readiness read must never mint enrollments, spawn the provider CLI, or
/// submit tasks.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PluginReadiness {
    /// True only when every gating layer is `ready`. Worker presence is
    /// informational, not gating: the lane acquires workers on demand.
    pub ready: bool,
    pub summary: String,
    pub layers: Vec<ReadinessLayer>,
    /// Informational worker facts (cached leases, live attachments).
    pub workers: serde_json::Value,
}

/// Pure assembly of the remote-compute readiness report; the live wrapper
/// [`plugin_readiness`] gathers the inputs. Split so tests pin every
/// branch without mutating process env.
pub(crate) fn remote_compute_readiness_from(
    environment_configured: bool,
    home_url: Result<String, String>,
    tls_terminated_proxy: bool,
    server_fingerprint_present: bool,
    cached_leases: usize,
    live_attached: usize,
) -> PluginReadiness {
    let environment = if environment_configured {
        ReadinessLayer {
            layer: LAYER_ENVIRONMENT,
            status: LayerStatus::Ready,
            detail: "INTENDANT_CODEX_CLOUD_ENVIRONMENT is set".to_string(),
            fix: None,
        }
    } else {
        ReadinessLayer {
            layer: LAYER_ENVIRONMENT,
            status: LayerStatus::Blocked,
            detail: "no Codex Cloud environment id is configured".to_string(),
            fix: Some(
                "set INTENDANT_CODEX_CLOUD_ENVIRONMENT to the Codex Cloud \
                 environment id (docs/src/codex-cloud-workers.md)"
                    .to_string(),
            ),
        }
    };
    let home = match home_url {
        Ok(url) => ReadinessLayer {
            layer: LAYER_HOME_URL,
            status: LayerStatus::Ready,
            detail: format!("workers will attach back to {url}"),
            fix: None,
        },
        Err(error) => ReadinessLayer {
            layer: LAYER_HOME_URL,
            status: LayerStatus::Blocked,
            detail: error,
            fix: Some(
                "set INTENDANT_CODEX_CLOUD_HOME_URL to the daemon's reachable \
                 wss:// URL"
                    .to_string(),
            ),
        },
    };
    let tls = if tls_terminated_proxy {
        ReadinessLayer {
            layer: LAYER_TLS_IDENTITY,
            status: LayerStatus::Ready,
            detail: "TLS-terminating proxy mode: attachment pinning is \
                     delegated to the explicitly trusted proxy"
                .to_string(),
            fix: None,
        }
    } else if server_fingerprint_present {
        ReadinessLayer {
            layer: LAYER_TLS_IDENTITY,
            status: LayerStatus::Ready,
            detail: "daemon gateway TLS identity present (workers pin its \
                     fingerprint)"
                .to_string(),
            fix: None,
        }
    } else {
        ReadinessLayer {
            layer: LAYER_TLS_IDENTITY,
            status: LayerStatus::Blocked,
            detail: "the daemon gateway TLS identity has not been minted yet".to_string(),
            fix: Some(
                "start the daemon with its web gateway enabled (the default) \
                 so it mints the TLS identity workers pin"
                    .to_string(),
            ),
        }
    };

    let layers = vec![environment, home, tls];
    let ready = layers
        .iter()
        .all(|layer| layer.status == LayerStatus::Ready);
    let not_ready: Vec<&str> = layers
        .iter()
        .filter(|layer| layer.status != LayerStatus::Ready)
        .map(|layer| layer.layer)
        .collect();
    let summary = if ready {
        "ready — remote commands can acquire Codex Cloud workers".to_string()
    } else {
        format!("needs setup: {}", not_ready.join(", "))
    };
    PluginReadiness {
        ready,
        summary,
        layers,
        workers: serde_json::json!({
            "cached_leases": cached_leases,
            "live_attached": live_attached,
        }),
    }
}

/// Live readiness for one bundled plugin id. Unknown ids report a
/// degenerate not-ready (callers validate ids first).
pub(crate) fn plugin_readiness(plugin_id: &str) -> PluginReadiness {
    match plugin_id {
        REMOTE_COMPUTE_PLUGIN_ID => {
            let environment_configured = std::env::var("INTENDANT_CODEX_CLOUD_ENVIRONMENT")
                .is_ok_and(|value| !value.trim().is_empty());
            let home_url = crate::codex_cloud_attach::home_url_from(None);
            let tls_terminated_proxy = crate::codex_cloud_attach::tls_terminated_proxy_from_env();
            let server_fingerprint_present = if tls_terminated_proxy {
                false
            } else {
                let cert_dir = crate::access::backend::select_backend().cert_dir();
                crate::access::certs::read_server_cert_fingerprint(&cert_dir).is_some()
            };
            let leases = crate::codex_cloud::cached_leases(&crate::codex_cloud::state_path())
                .unwrap_or_default();
            let live_attached = leases
                .iter()
                .filter(|lease| {
                    lease.attachment_state == crate::codex_cloud::AttachmentState::Connected
                        && crate::codex_cloud_attach::attachment_channel(&lease.task_id).is_some()
                })
                .count();
            remote_compute_readiness_from(
                environment_configured,
                home_url,
                tls_terminated_proxy,
                server_fingerprint_present,
                leases.len(),
                live_attached,
            )
        }
        _ => PluginReadiness {
            ready: false,
            summary: format!("unknown plugin '{plugin_id}'"),
            layers: Vec::new(),
            workers: serde_json::Value::Null,
        },
    }
}

// ── Active payloads (what the installer materializes) ───────────────────────

/// The skill payloads that should exist on disk right now: every skill of
/// every plugin that is both enabled and ready. One source of truth for
/// boot reconcile and the enable/disable handlers.
pub(crate) fn active_plugin_skills_in(
    state_root: &Path,
) -> Vec<(&'static str, &'static BuiltinSkill)> {
    let enabled = enabled_plugins_in(state_root);
    BUNDLED_PLUGINS
        .iter()
        .filter(|plugin| enabled.contains(plugin.id))
        .filter(|plugin| plugin_readiness(plugin.id).ready)
        .flat_map(|plugin| plugin.skills.iter().map(|skill| (plugin.id, skill)))
        .collect()
}

/// [`active_plugin_skills_in`] against the daemon's own state root.
pub(crate) fn active_plugin_skills() -> Vec<(&'static str, &'static BuiltinSkill)> {
    active_plugin_skills_in(&intendant_core::state_paths::intendant_home())
}

// ── Catalog (the one body the API serves) ───────────────────────────────────

/// Derived lifecycle headline for a catalog card.
fn plugin_state_label(enabled: bool, ready: bool, skills_settled: bool) -> &'static str {
    match (enabled, ready, skills_settled) {
        (false, _, _) => "available",
        (true, false, _) => "needs_setup",
        (true, true, true) => "enabled",
        (true, true, false) => "setup_failed",
    }
}

/// The full catalog as one JSON body: per plugin — identity, enabled flag,
/// derived lifecycle state, live readiness, and per-skill install facts
/// for both global roots. Everything the dashboard renders derives from
/// this body; the frontend holds no plugin vocabulary of its own.
fn entry_json(
    plugin: &'static BundledPlugin,
    enabled_set: &BTreeSet<String>,
    home: &Path,
) -> serde_json::Value {
    let enabled = enabled_set.contains(plugin.id);
    let readiness = plugin_readiness(plugin.id);
    // Settled = every root either carries our current copy or is
    // legitimately out of our hands (a user-owned root or dir). Only
    // consulted while enabled && ready; `absent`/`stale` there means the
    // last reconcile did not stick.
    let mut skills_settled = true;
    let skills: Vec<serde_json::Value> = plugin
        .skills
        .iter()
        .map(|skill| {
            let statuses = crate::skill_install::skill_install_status_in(home, skill);
            if !statuses.iter().all(|(_, status)| {
                matches!(*status, "installed" | "user_owned" | "root_user_owned")
            }) {
                skills_settled = false;
            }
            let roots: serde_json::Map<String, serde_json::Value> = statuses
                .into_iter()
                .map(|(root, status)| (root.to_string(), serde_json::json!(status)))
                .collect();
            serde_json::json!({ "name": skill.name, "roots": roots })
        })
        .collect();
    serde_json::json!({
        "id": plugin.id,
        "display_name": plugin.display_name,
        "summary": plugin.summary,
        "enabled": enabled,
        "state": plugin_state_label(enabled, readiness.ready, skills_settled),
        "readiness": readiness,
        "skills": skills,
    })
}

pub(crate) fn plugin_catalog_json_in(state_root: &Path, home: &Path) -> serde_json::Value {
    let enabled_set = enabled_plugins_in(state_root);
    let plugins: Vec<serde_json::Value> = BUNDLED_PLUGINS
        .iter()
        .map(|plugin| entry_json(plugin, &enabled_set, home))
        .collect();
    serde_json::json!({ "plugins": plugins })
}

/// One plugin's catalog entry (the same shape the list serves), or `None`
/// for an unknown id.
pub(crate) fn plugin_entry_json_in(
    state_root: &Path,
    home: &Path,
    plugin_id: &str,
) -> Option<serde_json::Value> {
    let plugin = bundled_plugin(plugin_id)?;
    Some(entry_json(plugin, &enabled_plugins_in(state_root), home))
}

/// [`plugin_catalog_json_in`] against the daemon's own roots.
pub(crate) fn plugin_catalog_json() -> serde_json::Value {
    let home = dirs::home_dir().unwrap_or_default();
    plugin_catalog_json_in(&intendant_core::state_paths::intendant_home(), &home)
}

/// [`plugin_entry_json_in`] against the daemon's own roots.
pub(crate) fn plugin_entry_json(plugin_id: &str) -> Option<serde_json::Value> {
    let home = dirs::home_dir().unwrap_or_default();
    plugin_entry_json_in(
        &intendant_core::state_paths::intendant_home(),
        &home,
        plugin_id,
    )
}

/// [`set_plugin_enabled_in`] against the daemon's own state root.
pub(crate) fn set_plugin_enabled(plugin_id: &str, enabled: bool) -> Result<(), String> {
    set_plugin_enabled_in(
        &intendant_core::state_paths::intendant_home(),
        plugin_id,
        enabled,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_table_matches_the_plugins_directory() {
        let plugins_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins");
        let mut on_disk: Vec<String> = std::fs::read_dir(&plugins_root)
            .expect("plugins/ readable")
            .flatten()
            .filter(|entry| {
                entry
                    .path()
                    .join(".codex-plugin")
                    .join("plugin.json")
                    .exists()
            })
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        on_disk.sort();
        let mut bundled: Vec<String> = BUNDLED_PLUGINS
            .iter()
            .map(|plugin| plugin.id.to_string())
            .collect();
        bundled.sort();
        assert_eq!(
            bundled, on_disk,
            "bundled plugin table drifted from plugins/"
        );

        for plugin in BUNDLED_PLUGINS {
            let package_root = plugins_root.join(plugin.id);
            let manifest_path = package_root.join(".codex-plugin").join("plugin.json");
            let manifest: serde_json::Value = serde_json::from_slice(
                &std::fs::read(&manifest_path).expect("plugin.json readable"),
            )
            .unwrap_or_else(|error| panic!("{} does not parse: {error}", manifest_path.display()));
            assert_eq!(
                manifest["name"].as_str(),
                Some(plugin.id),
                "plugin.json name must equal the package directory"
            );
            assert!(
                manifest["description"]
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty()),
                "plugin.json description must be non-empty"
            );
            let declared: Vec<&str> = manifest["skills"]
                .as_array()
                .expect("plugin.json skills array")
                .iter()
                .filter_map(|value| value.as_str())
                .collect();
            let embedded: Vec<String> = plugin
                .skills
                .iter()
                .map(|skill| format!("skills/{}", skill.name))
                .collect();
            assert_eq!(
                declared,
                embedded.iter().map(String::as_str).collect::<Vec<_>>(),
                "plugin.json skills must list exactly the embedded payloads"
            );

            for skill in plugin.skills {
                let disk = std::fs::read_to_string(
                    package_root
                        .join("skills")
                        .join(skill.name)
                        .join("SKILL.md"),
                )
                .expect("plugin skill SKILL.md readable");
                assert_eq!(
                    disk, skill.skill_md,
                    "embedded bytes for {}/{} are stale",
                    plugin.id, skill.name
                );
                let (config, _) =
                    intendant_core::skills::parse_skill_md(skill.skill_md, Path::new(skill.name))
                        .unwrap_or_else(|error| {
                            panic!(
                                "plugin skill {}/SKILL.md does not parse: {error}",
                                skill.name
                            )
                        });
                assert_eq!(
                    config.name, skill.name,
                    "frontmatter name must match the skill directory"
                );
                assert!(
                    !config.description.trim().is_empty(),
                    "plugin skill description must carry the ambient rule"
                );
            }
        }
    }

    #[test]
    fn plugin_skill_names_never_collide_with_builtins_or_each_other() {
        let mut seen = BTreeSet::new();
        for builtin in crate::builtin_skills::BUILTIN_SKILLS {
            seen.insert(builtin.name);
        }
        let mut ids = BTreeSet::new();
        for plugin in BUNDLED_PLUGINS {
            assert!(ids.insert(plugin.id), "duplicate plugin id {}", plugin.id);
            for skill in plugin.skills {
                assert!(
                    seen.insert(skill.name),
                    "plugin skill '{}' collides with a builtin or another plugin \
                     — both would fight over the same installed directory",
                    skill.name
                );
            }
        }
    }

    /// The dev-only `skills-internal/` tier must stay disjoint from every
    /// shipped skill name: `scripts/install-dev-skills.sh` symlinks those
    /// into the same global roots, and the daemon installer skips
    /// user-owned entries — so a name collision would silently block the
    /// shipped copy's install on every dev machine. Also pins hygiene:
    /// each internal SKILL.md parses and its frontmatter name equals the
    /// directory.
    #[test]
    fn internal_skills_stay_disjoint_from_shipped_names_and_parse() {
        let internal_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills-internal");
        let mut shipped = BTreeSet::new();
        for builtin in crate::builtin_skills::BUILTIN_SKILLS {
            shipped.insert(builtin.name);
        }
        for plugin in BUNDLED_PLUGINS {
            for skill in plugin.skills {
                shipped.insert(skill.name);
            }
        }
        let mut seen_any = false;
        for entry in std::fs::read_dir(&internal_root)
            .expect("skills-internal/ readable")
            .flatten()
        {
            let skill_md = entry.path().join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            seen_any = true;
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                !shipped.contains(name.as_str()),
                "skills-internal/{name} collides with a shipped skill — the \
                 dev symlink would block the daemon install of the shipped copy"
            );
            let body = std::fs::read_to_string(&skill_md).expect("internal SKILL.md readable");
            let (config, _) =
                intendant_core::skills::parse_skill_md(&body, Path::new(name.as_str()))
                    .unwrap_or_else(|error| {
                        panic!("skills-internal/{name}/SKILL.md does not parse: {error}")
                    });
            assert_eq!(
                config.name, name,
                "frontmatter name must match the directory"
            );
            assert!(
                !config.description.trim().is_empty(),
                "internal skill description must carry its trigger"
            );
        }
        assert!(seen_any, "skills-internal/ holds at least one skill");
    }

    #[test]
    fn enable_state_round_trips_and_preserves_foreign_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        assert!(
            enabled_plugins_in(root).is_empty(),
            "shipped default is off"
        );

        set_plugin_enabled_in(root, REMOTE_COMPUTE_PLUGIN_ID, true).unwrap();
        assert!(enabled_plugins_in(root).contains(REMOTE_COMPUTE_PLUGIN_ID));

        // A newer binary's id must survive this binary's rewrites.
        let path = plugin_state_path_in(root);
        let mut raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        raw["enabled"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("future-plugin"));
        std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();

        set_plugin_enabled_in(root, REMOTE_COMPUTE_PLUGIN_ID, false).unwrap();
        let after = enabled_plugins_in(root);
        assert!(!after.contains(REMOTE_COMPUTE_PLUGIN_ID));
        assert!(
            after.contains("future-plugin"),
            "foreign ids must be preserved: {after:?}"
        );

        set_plugin_enabled_in(root, REMOTE_COMPUTE_PLUGIN_ID, false)
            .expect("disable is idempotent");
        assert!(set_plugin_enabled_in(root, "no-such-plugin", true).is_err());
    }

    #[test]
    fn corrupt_or_foreign_version_state_reads_as_default() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let path = plugin_state_path_in(root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        assert!(enabled_plugins_in(root).is_empty());
        std::fs::write(
            &path,
            br#"{"version":99,"enabled":["codex-cloud-remote-compute"]}"#,
        )
        .unwrap();
        assert!(enabled_plugins_in(root).is_empty());
    }

    #[test]
    fn readiness_assembly_gates_on_the_three_layers_and_fails_closed() {
        let ready = remote_compute_readiness_from(
            true,
            Ok("wss://example.test:8765/api/codex-cloud/attach".to_string()),
            false,
            true,
            2,
            1,
        );
        assert!(ready.ready);
        assert_eq!(ready.layers.len(), 3);
        assert_eq!(ready.workers["live_attached"], 1);

        let proxy_mode = remote_compute_readiness_from(
            true,
            Ok("wss://example.test/api/codex-cloud/attach".to_string()),
            true,
            false,
            0,
            0,
        );
        assert!(
            proxy_mode.ready,
            "proxy mode must not require a local fingerprint"
        );

        let missing_env =
            remote_compute_readiness_from(false, Err("no url".to_string()), false, false, 0, 0);
        assert!(!missing_env.ready);
        assert!(missing_env.summary.contains(LAYER_ENVIRONMENT));
        assert!(missing_env.summary.contains(LAYER_HOME_URL));
        assert!(missing_env.summary.contains(LAYER_TLS_IDENTITY));
        assert!(
            missing_env
                .layers
                .iter()
                .all(
                    |layer| layer.status == LayerStatus::Blocked && layer.fix.is_some()
                        || layer.status == LayerStatus::Ready
                ),
            "blocked layers must carry a fix"
        );
    }

    #[test]
    fn state_labels_derive_from_the_three_facts() {
        assert_eq!(plugin_state_label(false, false, false), "available");
        assert_eq!(plugin_state_label(false, true, true), "available");
        assert_eq!(plugin_state_label(true, false, true), "needs_setup");
        assert_eq!(plugin_state_label(true, true, true), "enabled");
        assert_eq!(plugin_state_label(true, true, false), "setup_failed");
    }
}
